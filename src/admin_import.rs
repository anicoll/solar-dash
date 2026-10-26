//! The one-time copy of the admin data from `config.toml` into the database.
//!
//! Tariffs, petrol prices, purchases, past bills and a few preferences used
//! to be set in the config file. They are now edited on the Admin page and
//! kept in tables. On start-up `serve` copies whatever the config still has,
//! once per kind of data; the `admin_change` row with action `import` is the
//! mark that it has been done. After that the config's copy is ignored, and
//! a warning on every start says what to delete from it.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::{SqliteConnection, SqlitePool};
use tracing::{info, warn};

use crate::admin_data::{log_change, tariff_body};
use crate::config::{self, Config, parse_date};
use crate::{bills, petrol_prices};

/// What was copied: how many rows, and where from.
fn summary(rows: usize, source: &str) -> Value {
    json!({ "rows": rows, "source": source })
}

/// Whether `what` has been imported before.
async fn imported(conn: &mut SqliteConnection, what: &str) -> Result<bool> {
    let found: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM admin_change WHERE what = ? AND action = 'import' LIMIT 1")
            .bind(what)
            .fetch_optional(&mut *conn)
            .await
            .context("could not look for an earlier import")?;
    Ok(found.is_some())
}

/// Copies everything the config has that has not been imported yet. Each kind
/// of data is one transaction, and is marked as imported even when the config
/// has none, so emptying a table later never brings the config's copy back.
pub async fn import_from_config(pool: &SqlitePool, config: &Config) -> Result<()> {
    for what in ["tariffs", "petrol", "purchases", "bills", "settings"] {
        let mut tx = pool.begin().await?;
        if imported(&mut tx, what).await? {
            continue;
        }
        let (rows, source) = match what {
            "tariffs" => import_tariffs(&mut tx, config).await?,
            "petrol" => import_petrol(&mut tx, config).await?,
            "purchases" => import_purchases(&mut tx, config).await?,
            "bills" => import_bills(&mut tx, config).await?,
            _ => import_settings(&mut tx, config).await?,
        };
        log_change(
            &mut tx,
            what,
            "import",
            None,
            None,
            Some(&summary(rows, &source)),
        )
        .await?;
        tx.commit().await?;
        info!(what, rows, source, "imported from the config");
    }
    Ok(())
}

const CONFIG_TOML: &str = "config.toml";

async fn import_tariffs(conn: &mut SqliteConnection, config: &Config) -> Result<(usize, String)> {
    let Some(current) = &config.tariff else {
        return Ok((0, CONFIG_TOML.to_string()));
    };
    // Oldest first. A plan starts the day after the previous one ended; the
    // first has no known start.
    let mut plans: Vec<(Option<String>, config::Tariff)> = Vec::new();
    let mut start = None;
    for previous in &config.previous_tariff {
        let tariff = config::Tariff {
            name: previous
                .name
                .clone()
                .or_else(|| previous.tariff.name.clone()),
            ..previous.tariff.clone()
        };
        plans.push((start.map(|d: chrono::NaiveDate| d.to_string()), tariff));
        let until = parse_date(&previous.until)?;
        start = Some(until.succ_opt().context("until is too late")?);
    }
    plans.push((start.map(|d| d.to_string()), current.clone()));
    for (valid_from, tariff) in &plans {
        sqlx::query("INSERT INTO tariff_plan (valid_from, body) VALUES (?, ?)")
            .bind(valid_from)
            .bind(tariff_body(tariff)?)
            .execute(&mut *conn)
            .await
            .context("could not save a tariff plan")?;
    }
    Ok((plans.len(), CONFIG_TOML.to_string()))
}

async fn import_petrol(conn: &mut SqliteConnection, config: &Config) -> Result<(usize, String)> {
    let Some(teslamate) = &config.teslamate else {
        return Ok((0, CONFIG_TOML.to_string()));
    };
    let (changes, source) = match &teslamate.petrol_prices_file {
        // A bad file stops `serve` here, with the line in the message. A
        // missing file counts as empty, as it did before.
        Some(file) if std::path::Path::new(file).exists() => (
            petrol_prices::load(std::path::Path::new(file))?,
            file.clone(),
        ),
        Some(file) => (Vec::new(), file.clone()),
        None => {
            let changes = teslamate
                .petrol_price
                .iter()
                .map(|p| Ok((parse_date(&p.from)?, p.cents_per_litre)))
                .collect::<Result<Vec<_>>>()?;
            (changes, CONFIG_TOML.to_string())
        }
    };
    for (from, cents) in &changes {
        sqlx::query("INSERT INTO petrol_price (from_date, cents_per_litre) VALUES (?, ?)")
            .bind(from.to_string())
            .bind(cents)
            .execute(&mut *conn)
            .await
            .context("could not save a petrol price")?;
    }
    Ok((changes.len(), source))
}

async fn import_purchases(conn: &mut SqliteConnection, config: &Config) -> Result<(usize, String)> {
    for p in &config.system.purchase {
        sqlx::query(
            "INSERT INTO purchase (name, date, cost_dollars, battery_kwh) VALUES (?, ?, ?, ?)",
        )
        .bind(&p.name)
        .bind(&p.date)
        .bind(p.cost_dollars)
        .bind(p.battery_kwh)
        .execute(&mut *conn)
        .await
        .context("could not save a purchase")?;
    }
    Ok((config.system.purchase.len(), CONFIG_TOML.to_string()))
}

async fn import_bills(conn: &mut SqliteConnection, config: &Config) -> Result<(usize, String)> {
    let Some(section) = &config.bills else {
        return Ok((0, CONFIG_TOML.to_string()));
    };
    // A bad bills file stops `serve` here, with the line in the message.
    let list = bills::load(std::path::Path::new(&section.file))?;
    for b in &list {
        sqlx::query(
            "INSERT INTO bill (from_date, to_date, bill_total, excluded, import_kwh, \
             import_dollars, export_kwh, export_dollars, supply_dollars, note) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(b.from.to_string())
        .bind(b.to.to_string())
        .bind(b.bill_total)
        .bind(b.excluded)
        .bind(b.import_kwh)
        .bind(b.import_dollars)
        .bind(b.export_kwh)
        .bind(b.export_dollars)
        .bind(b.supply_dollars)
        .bind(&b.note)
        .execute(&mut *conn)
        .await
        .context("could not save a bill")?;
    }
    Ok((list.len(), section.file.clone()))
}

/// The settings the config sets, as `(key, JSON value)`.
fn config_settings(config: &Config) -> Vec<(&'static str, Value)> {
    let mut settings = Vec::new();
    if let Some(t) = &config.teslamate {
        if let Some(cents) = t.petrol_cents_per_litre {
            settings.push(("petrol_cents_per_litre", json!(cents)));
        }
        settings.push(("petrol_litres_per_100km", json!(t.petrol_litres_per_100km)));
        if let Some(cents) = t.away_cents_per_kwh {
            settings.push(("away_cents_per_kwh", json!(cents)));
        }
        if !t.compare_cars.is_empty() {
            settings.push(("compare_cars", json!(t.compare_cars)));
        }
        settings.push(("petrol_grade", json!(t.petrol_grade)));
    }
    settings.push((
        "price_rise_percent",
        json!(config.system.price_rise_percent),
    ));
    settings
}

async fn import_settings(conn: &mut SqliteConnection, config: &Config) -> Result<(usize, String)> {
    let settings = config_settings(config);
    for (key, value) in &settings {
        sqlx::query("INSERT INTO setting (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(value.to_string())
            .execute(&mut *conn)
            .await
            .context("could not save a setting")?;
    }
    Ok((settings.len(), CONFIG_TOML.to_string()))
}

/// What `config.toml` still has that is now ignored, one sentence each.
/// Settings that equal their default are not counted: they cannot be told
/// apart from being left out.
pub fn ignored_in_config(config: &Config) -> Vec<String> {
    let mut found = Vec::new();
    if config.tariff.is_some() || !config.previous_tariff.is_empty() {
        found.push(format!(
            "[tariff] and {} [[previous_tariff]] are ignored: tariffs are now edited on the \
             Admin page; delete them from config.toml",
            config.previous_tariff.len()
        ));
    }
    if let Some(t) = &config.teslamate {
        if t.petrol_prices_file.is_some() || !t.petrol_price.is_empty() {
            found.push(
                "petrol_prices_file and [[teslamate.petrol_price]] are ignored: petrol prices \
                 are now edited on the Admin page; delete them from config.toml"
                    .to_string(),
            );
        }
        let mut settings = Vec::new();
        for (name, set) in [
            ("petrol_cents_per_litre", t.petrol_cents_per_litre.is_some()),
            ("away_cents_per_kwh", t.away_cents_per_kwh.is_some()),
            ("compare_cars", !t.compare_cars.is_empty()),
            (
                "petrol_grade",
                t.petrol_grade != config::default_petrol_grade(),
            ),
            (
                "petrol_litres_per_100km",
                t.petrol_litres_per_100km != config::default_petrol_litres_per_100km(),
            ),
        ] {
            if set {
                settings.push(name);
            }
        }
        if !settings.is_empty() {
            found.push(format!(
                "{} in [teslamate] are ignored: settings are now edited on the Admin page; \
                 delete them from config.toml",
                settings.join(", ")
            ));
        }
    }
    if !config.system.purchase.is_empty() {
        found.push(format!(
            "{} [[system.purchase]] are ignored: purchases are now edited on the Admin page; \
             delete them from config.toml",
            config.system.purchase.len()
        ));
    }
    if config.system.price_rise_percent != config::default_price_rise_percent() {
        found.push(
            "price_rise_percent in [system] is ignored: settings are now edited on the Admin \
             page; delete it from config.toml"
                .to_string(),
        );
    }
    if config.bills.is_some() {
        found.push(
            "[bills] is ignored: past bills are now edited on the Admin page; delete it from \
             config.toml"
                .to_string(),
        );
    }
    found
}

/// Logs a warning for everything in [`ignored_in_config`].
pub fn warn_ignored(config: &Config) {
    for message in ignored_in_config(config) {
        warn!("config.toml {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_data::AdminData;
    use crate::cost::TariffSchedule;
    use crate::db;

    const BASE: &str = r#"
        [isolarcloud]
        appkey = "k"
        access_key = "s"
        rsa_public_key = "r"
        username = "u"
        password = "p"
    "#;

    /// A config with a bit of everything; `dir` holds the bills file.
    fn full_config(dir: &std::path::Path) -> Config {
        let bills = dir.join("bills.csv");
        std::fs::write(
            &bills,
            "from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,\
             export_dollars,supply_dollars,note\n\
             2026-01-01,2026-03-31,100.5,0,500,150,200,20,30.5,first, with comma\n",
        )
        .unwrap();
        let text = format!(
            r#"{BASE}
            [tariff]
            name = "Now"
            supply_cents_per_day = 100.0
            default_import_cents = 30.0
            import = [{{ from = "11:00", to = "14:00", cents = 0.0 }}]
            [[previous_tariff]]
            name = "Old"
            until = "2024-06-30"
            supply_cents_per_day = 90.0
            default_import_cents = 20.0
            [[previous_tariff]]
            until = "2025-06-30"
            supply_cents_per_day = 95.0
            default_import_cents = 25.0
            [system]
            price_rise_percent = 4.5
            [[system.purchase]]
            name = "Panels"
            date = "2024-01-02"
            cost_dollars = 9000.0
            [[system.purchase]]
            name = "Battery"
            date = "2025-03-04"
            cost_dollars = 5000.0
            battery_kwh = 10.0
            [bills]
            file = {bills:?}
            [teslamate]
            host = "localhost"
            petrol_cents_per_litre = 190.0
            petrol_litres_per_100km = 8.0
            compare_cars = {{ Y = "RAV4" }}
            [[teslamate.petrol_price]]
            from = "2026-01-01"
            cents_per_litre = 200.0
            "#
        );
        toml::from_str(&text).unwrap()
    }

    async fn open(dir: &std::path::Path) -> SqlitePool {
        db::open(dir.join("test.db").to_str().unwrap())
            .await
            .unwrap()
    }

    async fn load(pool: &SqlitePool) -> AdminData {
        AdminData::load(&mut pool.acquire().await.unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_config_is_copied_once_and_loads_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(dir.path()).await;
        let config = full_config(dir.path());
        import_from_config(&pool, &config).await.unwrap();

        let data = load(&pool).await;
        let from_config =
            TariffSchedule::from_config(config.tariff.as_ref().unwrap(), &config.previous_tariff)
                .unwrap();
        let loaded = data.tariff.as_ref().unwrap();
        assert_eq!(loaded.plan_spans(), from_config.plan_spans());
        // The cost cache key is unchanged for every plan, so stored costs stay valid.
        for date in ["2024-01-01", "2024-07-01", "2025-07-01", "2026-01-01"] {
            let day = date.parse().unwrap();
            assert_eq!(loaded.for_date(day).key(), from_config.for_date(day).key());
        }
        assert_eq!(data.purchases.len(), 2);
        assert_eq!(data.purchases[1].battery_kwh, Some(10.0));
        assert_eq!(data.price_rise_percent, 4.5);
        assert_eq!(data.bills.len(), 1);
        assert_eq!(data.bills[0].note, "first, with comma");
        assert_eq!(data.ev_prices.petrol_cents_per_litre, Some(190.0));
        assert_eq!(data.ev_prices.petrol_litres_per_100km, 8.0);
        assert_eq!(data.compare_cars["Y"], "RAV4");
        assert_eq!(data.petrol.on("2025-12-31"), Some(190.0));
        assert_eq!(data.petrol.on("2026-01-01"), Some(200.0));

        // Five imports are recorded, and a second start adds nothing.
        import_from_config(&pool, &config).await.unwrap();
        let log: Vec<(String, String)> = sqlx::query_as(
            "SELECT what, after FROM admin_change WHERE action = 'import' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let whats: Vec<&str> = log.iter().map(|(what, _)| what.as_str()).collect();
        assert_eq!(
            whats,
            ["tariffs", "petrol", "purchases", "bills", "settings"]
        );
        assert_eq!(log[0].1, r#"{"rows":3,"source":"config.toml"}"#);
    }

    #[tokio::test]
    async fn emptying_a_table_does_not_bring_the_config_back() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(dir.path()).await;
        let config = full_config(dir.path());
        import_from_config(&pool, &config).await.unwrap();
        sqlx::query("DELETE FROM tariff_plan")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM purchase")
            .execute(&pool)
            .await
            .unwrap();
        import_from_config(&pool, &config).await.unwrap();
        let data = load(&pool).await;
        assert!(data.tariff.is_none());
        assert!(data.purchases.is_empty());
    }

    #[tokio::test]
    async fn an_empty_config_still_marks_everything_imported() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open(dir.path()).await;
        let config: Config = toml::from_str(BASE).unwrap();
        import_from_config(&pool, &config).await.unwrap();
        let (imports,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM admin_change WHERE action = 'import'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(imports, 5);
        let data = load(&pool).await;
        assert!(data.tariff.is_none() && data.bills.is_empty());
        assert_eq!(data.petrol_grade, "Unleaded 91");
        assert!(ignored_in_config(&config).is_empty());
    }

    #[test]
    fn the_leftover_config_is_named_in_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let found = ignored_in_config(&full_config(dir.path()));
        assert!(found[0].starts_with("[tariff] and 2 [[previous_tariff]] are ignored"));
        let all = found.join("\n");
        for part in [
            "petrol_price",
            "compare_cars",
            "[[system.purchase]]",
            "[bills]",
        ] {
            assert!(all.contains(part), "{part}: {all}");
        }
    }
}
