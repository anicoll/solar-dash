//! The data the admin page edits (tariffs, petrol prices, purchases, bills and
//! a few preferences). It lives in SQLite tables; the rest of the app uses a
//! snapshot of it ([`AdminData`]) that is swapped whenever the tables change,
//! so edits apply without a restart. Handlers take the current snapshot once
//! per request.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::Value;
use sqlx::SqliteConnection;

use crate::bills::PastBill;
use crate::config::{self, parse_date};
use crate::cost::TariffSchedule;
use crate::db;
use crate::ev_cost::{EvPrices, PetrolHistory};
use crate::petrol_prices::check_price;
use crate::web::{ApiError, AppState, Purchase, purchases_from_config};

/// The data the admin page edits, as the rest of the app uses it.
#[derive(Debug, Clone)]
pub struct AdminData {
    /// The plans over time; `None` turns the cost features off.
    pub tariff: Option<Arc<TariffSchedule>>,
    /// The petrol and away-from-home prices.
    pub ev_prices: EvPrices,
    /// What petrol cost on each day.
    pub petrol: PetrolHistory,
    /// The petrol car each EV model is compared with.
    pub compare_cars: BTreeMap<String, String>,
    pub petrol_grade: String,
    /// Oldest first; empty turns the payback API off.
    pub purchases: Arc<[Purchase]>,
    /// Assumed yearly electricity price rise (percent) in the payback projection.
    pub price_rise_percent: f64,
    /// The past bills, oldest first; empty when there are none.
    pub bills: Arc<[PastBill]>,
    /// Grid import (kW) from which the Today page warns about the main breaker.
    pub grid_pull_warn_kw: f64,
    /// Grid import (kW) the owner wants to stay under.
    pub grid_pull_limit_kw: f64,
}

impl Default for AdminData {
    /// Nothing entered: cost features off, with the usual defaults.
    fn default() -> Self {
        Self {
            tariff: None,
            ev_prices: EvPrices::default(),
            petrol: PetrolHistory::default(),
            compare_cars: BTreeMap::new(),
            petrol_grade: "Unleaded 91".to_string(),
            purchases: Arc::from([]),
            price_rise_percent: 3.0,
            bills: Arc::from([]),
            grid_pull_warn_kw: 12.0,
            grid_pull_limit_kw: 15.0,
        }
    }
}

/// The keys of the `setting` table. A missing key means its default.
pub const SETTING_KEYS: [&str; 8] = [
    "petrol_cents_per_litre",
    "petrol_litres_per_100km",
    "away_cents_per_kwh",
    "compare_cars",
    "petrol_grade",
    "price_rise_percent",
    "grid_pull_warn_kw",
    "grid_pull_limit_kw",
];

/// The JSON kept in `tariff_plan.body`: the plan as in the config file, plus
/// its `name`. (`config::Tariff` leaves the name out when serialised, because
/// it is not part of the rates and must stay out of the cost cache key.)
#[derive(Serialize)]
struct TariffBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: &'a Option<String>,
    #[serde(flatten)]
    tariff: &'a config::Tariff,
}

/// The text to store in `tariff_plan.body`.
pub fn tariff_body(tariff: &config::Tariff) -> Result<String> {
    let body = TariffBody {
        name: &tariff.name,
        tariff,
    };
    serde_json::to_string(&body).context("could not write the tariff as JSON")
}

/// Reads a `tariff_plan.body` back. It gives the same cost cache key as the
/// tariff it was written from.
pub fn tariff_from_body(body: &str) -> Result<config::Tariff> {
    serde_json::from_str(body).context("the tariff is not valid JSON")
}

impl AdminData {
    /// Builds the snapshot from the tables, with clear errors for bad rows.
    /// Also used to check a whole set of changes before they are committed.
    pub async fn load(conn: &mut SqliteConnection) -> Result<Self> {
        let mut data = Self::default();

        let plans = load_plans(conn).await?;
        if !plans.is_empty() {
            let schedule =
                TariffSchedule::from_plans(&plans).context("the tariff plans do not work")?;
            data.tariff = Some(Arc::new(schedule));
        }

        for (key, value) in &load_settings(conn).await? {
            apply_setting(&mut data, key, value)?;
        }
        // Each threshold is checked alone by `apply_setting`; only here are
        // both known, so this also catches a change to just one of them.
        data.check_grid_pull()?;

        // The setting above is the price before the first `petrol_price` row.
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT from_date, cents_per_litre FROM petrol_price ORDER BY from_date",
        )
        .fetch_all(&mut *conn)
        .await
        .context("could not read the petrol prices")?;
        let mut changes = Vec::new();
        for (from, cents) in rows {
            let date = parse_date(&from).context("petrol price date")?;
            check_price(cents).with_context(|| format!("petrol price from {from}"))?;
            changes.push((date, cents));
        }
        data.petrol = PetrolHistory::new(data.ev_prices.petrol_cents_per_litre, changes);

        let rows: Vec<(String, String, f64, Option<f64>)> = sqlx::query_as(
            "SELECT name, date, cost_dollars, battery_kwh FROM purchase ORDER BY date, id",
        )
        .fetch_all(&mut *conn)
        .await
        .context("could not read the purchases")?;
        let mut list = Vec::new();
        for (i, (name, date, cost_dollars, battery_kwh)) in rows.into_iter().enumerate() {
            let purchase = config::Purchase {
                name,
                date,
                cost_dollars,
                battery_kwh,
            };
            purchase.check(&format!("purchase[{i}]"))?;
            list.push(purchase);
        }
        data.purchases = purchases_from_config(&list)?.into();

        data.bills = load_bills(conn).await?.into();
        Ok(data)
    }
}

/// The plans in `tariff_plan`, earliest first, as `(valid_from, tariff)`.
async fn load_plans(
    conn: &mut SqliteConnection,
) -> Result<Vec<(Option<NaiveDate>, config::Tariff)>> {
    let rows = load_plan_rows(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(_, valid_from, tariff)| (valid_from, tariff))
        .collect())
}

/// Like [`load_plans`], with each plan's `id` as the first part.
pub(crate) async fn load_plan_rows(
    conn: &mut SqliteConnection,
) -> Result<Vec<(i64, Option<NaiveDate>, config::Tariff)>> {
    let rows: Vec<(i64, Option<String>, String)> =
        sqlx::query_as("SELECT id, valid_from, body FROM tariff_plan ORDER BY valid_from, id")
            .fetch_all(&mut *conn)
            .await
            .context("could not read the tariff plans")?;
    let mut plans = Vec::new();
    for (id, valid_from, body) in rows {
        let valid_from = valid_from
            .map(|text| parse_date(&text))
            .transpose()
            .with_context(|| format!("valid_from of tariff plan #{id}"))?;
        let tariff = tariff_from_body(&body).with_context(|| format!("tariff plan #{id}"))?;
        tariff.check(&format!("tariff plan #{id}"))?;
        plans.push((id, valid_from, tariff));
    }
    Ok(plans)
}

/// The rows of the `setting` table as JSON values, by key.
pub(crate) async fn load_settings(conn: &mut SqliteConnection) -> Result<BTreeMap<String, Value>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM setting")
        .fetch_all(&mut *conn)
        .await
        .context("could not read the settings")?;
    rows.into_iter()
        .map(|(key, text)| {
            let value = serde_json::from_str(&text)
                .with_context(|| format!("setting {key} is not valid JSON"))?;
            Ok((key, value))
        })
        .collect()
}

/// The bills in the `bill` table, oldest first, checked like the CSV file.
pub(crate) async fn load_bills(conn: &mut SqliteConnection) -> Result<Vec<PastBill>> {
    type Row = (String, String, f64, f64, f64, f64, f64, f64, f64, String);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT from_date, to_date, bill_total, excluded, import_kwh, import_dollars, \
         export_kwh, export_dollars, supply_dollars, note FROM bill ORDER BY from_date",
    )
    .fetch_all(&mut *conn)
    .await
    .context("could not read the bills")?;
    let mut bills: Vec<PastBill> = Vec::new();
    for row in rows {
        let bill = PastBill {
            from: parse_date(&row.0).context("bill from")?,
            to: parse_date(&row.1).context("bill to")?,
            bill_total: row.2,
            excluded: row.3,
            import_kwh: row.4,
            import_dollars: row.5,
            export_kwh: row.6,
            export_dollars: row.7,
            supply_dollars: row.8,
            note: row.9,
        };
        bill.check()
            .with_context(|| format!("bill from {}", bill.from))?;
        if let Some(previous) = bills.last()
            && bill.from <= previous.to
        {
            bail!(
                "the bill {} to {} overlaps the one ending {}",
                bill.from,
                bill.to,
                previous.to
            );
        }
        bills.push(bill);
    }
    Ok(bills)
}

impl AdminData {
    /// The warning level must be below the limit.
    fn check_grid_pull(&self) -> Result<()> {
        anyhow::ensure!(
            self.grid_pull_warn_kw < self.grid_pull_limit_kw,
            "grid_pull_warn_kw ({}) must be less than grid_pull_limit_kw ({})",
            self.grid_pull_warn_kw,
            self.grid_pull_limit_kw
        );
        Ok(())
    }
}

/// Checks one setting's JSON value and applies it to `data`. Used for every
/// key by `load`, and by the API to validate a new value.
pub fn apply_setting(data: &mut AdminData, key: &str, value: &Value) -> Result<()> {
    let positive = || -> Result<f64> {
        let number = value.as_f64().filter(|n| *n > 0.0 && n.is_finite());
        number.with_context(|| format!("{key} must be a number more than 0"))
    };
    let optional_positive = || -> Result<Option<f64>> {
        if value.is_null() {
            Ok(None)
        } else {
            positive().map(Some)
        }
    };
    match key {
        "petrol_cents_per_litre" => data.ev_prices.petrol_cents_per_litre = optional_positive()?,
        "petrol_litres_per_100km" => data.ev_prices.petrol_litres_per_100km = positive()?,
        "away_cents_per_kwh" => data.ev_prices.away_cents_per_kwh = optional_positive()?,
        "compare_cars" => {
            let cars: BTreeMap<String, String> = serde_json::from_value(value.clone())
                .with_context(|| format!("{key} must be an object of model to car name"))?;
            for (model, car) in &cars {
                anyhow::ensure!(
                    !model.trim().is_empty() && !car.trim().is_empty(),
                    "{key} needs a model and a car name in every entry"
                );
            }
            data.compare_cars = cars;
        }
        "petrol_grade" => {
            let grade = value
                .as_str()
                .filter(|g| !g.trim().is_empty())
                .with_context(|| format!("{key} must be a non-empty string"))?;
            data.petrol_grade = grade.to_string();
        }
        "price_rise_percent" => {
            let percent = value
                .as_f64()
                .filter(|p| (0.0..=20.0).contains(p))
                .with_context(|| format!("{key} must be a number between 0 and 20"))?;
            data.price_rise_percent = percent;
        }
        "grid_pull_warn_kw" => data.grid_pull_warn_kw = positive()?,
        "grid_pull_limit_kw" => data.grid_pull_limit_kw = positive()?,
        _ => bail!("unknown setting {key:?}"),
    }
    Ok(())
}

/// A shared, swappable handle to the current [`AdminData`]. Cloning shares
/// the same slot.
#[derive(Clone)]
pub struct SharedAdminData {
    slot: Arc<RwLock<Arc<AdminData>>>,
    /// Held for the whole of a write (see [`write`]), so two writes cannot
    /// swap their snapshots in the wrong order.
    writing: Arc<tokio::sync::Mutex<()>>,
}

impl SharedAdminData {
    pub fn new(data: AdminData) -> Self {
        Self {
            slot: Arc::new(RwLock::new(Arc::new(data))),
            writing: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The current data. Cheap: the lock is only held to clone the `Arc`, and
    /// the snapshot stays the same for as long as the caller keeps it.
    pub fn get(&self) -> Arc<AdminData> {
        // A poisoned lock still holds a whole `Arc`, so keep using it.
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Swaps in new data; requests already running keep the old snapshot.
    pub fn set(&self, data: AdminData) {
        *self.slot.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(data);
    }
}

/// Records one change in `admin_change`. `before` and `after` are the row as
/// JSON (`None` where there was no row).
pub async fn log_change(
    conn: &mut SqliteConnection,
    what: &str,
    action: &str,
    key: Option<&str>,
    before: Option<&Value>,
    after: Option<&Value>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO admin_change (at, what, action, key, before, after) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(db::now())
    .bind(what)
    .bind(action)
    .bind(key)
    .bind(before.map(Value::to_string))
    .bind(after.map(Value::to_string))
    .execute(&mut *conn)
    .await
    .context("could not record the change")?;
    Ok(())
}

/// Makes a change to the admin tables and swaps in the new snapshot.
///
/// `change` runs inside a transaction and writes the rows and their
/// `admin_change` entries. Then the whole set is loaded again to check it
/// (any error rolls everything back and becomes a 400 with the message), the
/// transaction is committed and the shared snapshot is replaced.
pub async fn write<F>(state: &AppState, change: F) -> Result<(), ApiError>
where
    F: AsyncFnOnce(&mut SqliteConnection) -> Result<(), ApiError>,
{
    let shared = state.admin();
    let _one_at_a_time = shared.writing.lock().await;
    let mut tx = state.pool().begin().await.map_err(anyhow::Error::from)?;
    change(&mut tx).await?;
    let data = AdminData::load(&mut tx)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    shared.set(data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn set_swaps_for_every_clone_but_not_for_old_snapshots() {
        let shared = SharedAdminData::new(AdminData::default());
        let other = shared.clone();
        let before = shared.get();
        other.set(AdminData {
            price_rise_percent: 5.0,
            ..AdminData::default()
        });
        assert_eq!(shared.get().price_rise_percent, 5.0);
        assert_eq!(before.price_rise_percent, 3.0);
    }

    #[test]
    fn grid_pull_settings_are_positive_and_warn_is_below_limit() {
        let mut data = AdminData::default();
        assert_eq!(
            (data.grid_pull_warn_kw, data.grid_pull_limit_kw),
            (12.0, 15.0)
        );
        apply_setting(&mut data, "grid_pull_warn_kw", &json!(10.5)).unwrap();
        apply_setting(&mut data, "grid_pull_limit_kw", &json!(14)).unwrap();
        assert_eq!(
            (data.grid_pull_warn_kw, data.grid_pull_limit_kw),
            (10.5, 14.0)
        );
        data.check_grid_pull().unwrap();

        for key in ["grid_pull_warn_kw", "grid_pull_limit_kw"] {
            for bad in [json!(0), json!(-1), json!("12"), json!(null)] {
                assert!(apply_setting(&mut data, key, &bad).is_err(), "{key} {bad}");
            }
        }
        // Equal or reversed levels fail the whole-set check.
        data.grid_pull_warn_kw = 14.0;
        assert!(data.check_grid_pull().is_err());
        data.grid_pull_warn_kw = 15.0;
        assert!(data.check_grid_pull().is_err());
    }

    async fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = crate::web::hash_password("pw").unwrap();
        let tz = chrono_tz::Australia::Adelaide;
        let key = axum_extra::extract::cookie::Key::generate();
        (dir, AppState::new(pool, 1, tz, &hash, key).unwrap())
    }

    #[tokio::test]
    async fn write_swaps_the_snapshot_and_a_bad_set_rolls_back() {
        let (_dir, state) = test_state().await;
        const PLAN: &str = r#"INSERT INTO tariff_plan (valid_from, body) VALUES (NULL,
            '{"supply_cents_per_day":100.0,"default_import_cents":30.0,"zero_import_credit":null}')"#;
        write(&state, async |tx| {
            sqlx::query(PLAN).execute(&mut *tx).await.unwrap();
            log_change(tx, "tariffs", "create", Some("1"), None, None).await?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(state.admin().get().tariff.is_some());

        // A second plan with no start date is not allowed: nothing is kept, not even
        // the change log entry.
        let result = write(&state, async |tx| {
            // SQLite allows several NULLs in a UNIQUE column; the load rejects them.
            sqlx::query(PLAN).execute(&mut *tx).await.unwrap();
            log_change(tx, "tariffs", "create", Some("2"), None, None).await?;
            Ok(())
        })
        .await;
        let message = format!("{:?}", result.unwrap_err());
        assert!(message.contains("tariff plans do not work"), "{message}");
        let (plans,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tariff_plan")
            .fetch_one(state.pool())
            .await
            .unwrap();
        let (changes,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM admin_change")
            .fetch_one(state.pool())
            .await
            .unwrap();
        assert_eq!((plans, changes), (1, 1));
        assert!(state.admin().get().tariff.is_some());
    }

    /// A plan that goes through the table (body JSON) must give the same
    /// cost cache key as the one in the config, or stored costs go stale.
    #[test]
    fn a_tariff_keeps_its_cache_key_through_the_body_json() {
        let from_toml: config::Tariff = toml::from_str(
            r#"
            name = "ZeroHero"
            supply_cents_per_day = 150.0
            default_import_cents = 40.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "21:00", to = "06:00", cents = 33.333 },
            ]
            default_export_cents = 0.1
            export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
            export_tier = { first_kwh_per_day = 10.0, cents = 7.5 }
            [zero_import_credit]
            from = "11:00"
            to = "14:00"
            max_kwh = 0.03
            credit_cents = 100.0
            "#,
        )
        .unwrap();
        let body = tariff_body(&from_toml).unwrap();
        // The name is in the body even though it is not in the key.
        assert!(body.contains("ZeroHero"));
        let back = tariff_from_body(&body).unwrap();
        assert_eq!(back.name.as_deref(), Some("ZeroHero"));
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&from_toml).unwrap()
        );
        // And through the schedule, which is what the cache reads.
        let before = TariffSchedule::from_plans(&[(None, from_toml)]).unwrap();
        let after = TariffSchedule::from_plans(&[(None, back)]).unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert_eq!(before.for_date(day).key(), after.for_date(day).key());
    }
}
