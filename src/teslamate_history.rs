//! The TeslaMate history sync: a task inside `serve` that copies summaries of
//! TeslaMate's history from its Postgres database into ours.
//!
//! The live MQTT reader (see `teslamate`) only knows the present. This reads
//! the past, for every car TeslaMate has. Only summaries are kept, never
//! TeslaMate's raw rows. The login only needs SELECT on `cars`, `drives`,
//! `charging_processes`, `charges` and `geofences`, and nothing is ever
//! written to TeslaMate.
//!
//! One run, per car:
//!
//! - **drives** become one row per local day (`tm_drive_daily`: km and number
//!   of drives), added up in Postgres. The days to redo are those of drives
//!   newer than the last id seen (kept in `sync_state`) or still going, plus
//!   the last few days. A day is always recomputed in full. The first run has
//!   no last id, so it does every day.
//! - **charging sessions** (`tm_charge`) with an id above the newest stored
//!   one, plus stored sessions that had not ended when last read. Whether a
//!   fast charger was used is worked out in Postgres from the session's
//!   readings.
//!
//! Ids are looked at per car, not overall: a car that was re-added has its
//! history under much higher ids, so one overall maximum would hide the new
//! car's drives.
//!
//! The minute-by-minute charger power is not copied. [`charge_power`] reads
//! it from Postgres when a session needs costing, and it is thrown away once
//! the session is costed.
//!
//! **Costing** comes last in a run, and only with a `[tariff]`. A session at
//! home (geofence "Home") that ended at least 2 hours ago is costed once from
//! the house's energy (see `ev_cost::historic_session_cost`) and the result is
//! stored on its row. It is costed again if the plan, the bills or the costing
//! rules change. A session the house data covers by less than 98% waits up to
//! 3 days for the data to arrive. A run costs at most 50 sessions, so the first
//! backfill is spread over a few runs.
//!
//! When it runs: 30 seconds after startup, then every 15 minutes. A run opens
//! a small connection pool and closes it at the end, so nothing stays
//! connected in between. If the database cannot be reached the run is logged
//! as a warning and the next one tries again. The URL holds a password and is
//! never logged.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Days, Utc};
use chrono_tz::Tz;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tracing::{info, warn};

use crate::db::{self, TmCar, TmCharge, TmChargeCost, TmChargeToCost, TmDriveDay};
use crate::ev_cost::{self, CostSetup};
use crate::web;

/// How long after startup the first run happens.
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// How long between runs.
const RUN_EVERY: Duration = Duration::from_secs(15 * 60);
/// Every run redoes this many of the latest local days (today included), so a
/// drive that ended after the last run is counted.
const RECENT_DAYS: u64 = 3;
/// How long to wait for a connection to TeslaMate's database.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A session is costed once it has been over this long, so the house's last
/// readings have been saved.
const COST_AFTER_SECS: i64 = 2 * 3600;
/// A session with less house data than this share waits for more...
const MIN_COVERAGE: f64 = 0.98;
/// ...until it has been over this long, and is then costed with what there is.
const WAIT_FOR_DATA_SECS: i64 = 3 * 86_400;
/// The most sessions one run costs.
const MAX_COSTED_PER_RUN: usize = 50;

/// What one run did, for the log line.
#[derive(Debug, Default, PartialEq)]
struct Counts {
    cars: usize,
    drive_days: usize,
    charges_new: usize,
    charges_updated: usize,
}

// ---------------------------------------------------------------------------
// The pure parts
// ---------------------------------------------------------------------------

/// One row of TeslaMate's `charges` table, as far as we use it.
#[derive(Debug, Clone, PartialEq)]
struct ChargeReading {
    ts: i64,
    /// The charger power, whole kW (TeslaMate rounds it).
    charger_power_kw: Option<f64>,
    charger_actual_current: Option<f64>,
    charger_voltage: Option<f64>,
    charger_phases: Option<f64>,
}

/// The charger power in watts. Current times voltage times phases is exact,
/// so it is used when all three are there and above 0; otherwise the rounded
/// kW figure is the fallback. `None` if neither works.
fn power_w(r: &ChargeReading) -> Option<f64> {
    if let (Some(a), Some(v), Some(p)) = (
        r.charger_actual_current,
        r.charger_voltage,
        r.charger_phases,
    ) && a > 0.0
        && v > 0.0
        && p > 0.0
    {
        return Some(a * v * p);
    }
    // `>= 0.0` is false for NaN too. A reading of 0 kW is a real reading.
    r.charger_power_kw
        .filter(|kw| *kw >= 0.0)
        .map(|kw| kw * 1000.0)
}

/// The power curve of a session: `(ts, watts)` for every reading that has a power.
fn power_curve(readings: &[ChargeReading]) -> Vec<(i64, f64)> {
    readings
        .iter()
        .filter_map(|r| power_w(r).map(|w| (r.ts, w)))
        .collect()
}

/// The sessions that need costing now, newest first: those whose stored key
/// is not the one they would get today.
fn needing_cost(
    sessions: Vec<TmChargeToCost>,
    setup: &CostSetup,
    tz: Tz,
) -> Vec<(TmChargeToCost, String)> {
    sessions
        .into_iter()
        .filter_map(|s| {
            let key = ev_cost::historic_cost_key(&setup.schedule, &setup.bills, tz, s.started_at);
            (s.cost_key.as_deref() != Some(key.as_str())).then_some((s, key))
        })
        .collect()
}

/// Whether to leave a session for a later run: the house data covers too
/// little of it and it ended recently enough that more may still arrive.
fn should_wait(coverage: f64, ended_at: i64, now: i64) -> bool {
    coverage < MIN_COVERAGE && ended_at > now - WAIT_FOR_DATA_SECS
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

/// The `cars` columns that describe how a car is specified. Some TeslaMate
/// versions lack some of them.
const CAR_DETAIL_COLUMNS: [&str; 4] = [
    "trim_badging",
    "exterior_color",
    "wheel_type",
    "marketing_name",
];

/// The query for TeslaMate's cars: the columns in [`CAR_DETAIL_COLUMNS`] that
/// `existing` lists are selected, the others come back as NULL.
fn cars_query(existing: &[String]) -> String {
    let details: Vec<String> = CAR_DETAIL_COLUMNS
        .iter()
        .map(|col| {
            if existing.iter().any(|e| e == col) {
                format!("{col}::text AS {col}")
            } else {
                format!("NULL::text AS {col}")
            }
        })
        .collect();
    format!(
        "SELECT id::bigint AS id, name, model, {} FROM cars ORDER BY id",
        details.join(", ")
    )
}

/// Reads TeslaMate's cars. The query is built from fixed column names only.
async fn fetch_cars(pg: &PgPool) -> Result<Vec<TmCar>> {
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'cars' AND table_schema = ANY(current_schemas(false))",
    )
    .fetch_all(pg)
    .await
    .context("could not read the columns of TeslaMate's cars")?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(cars_query(&existing)))
        .fetch_all(pg)
        .await
        .context("could not read TeslaMate's cars")?;
    rows.iter()
        .map(|row| {
            Ok(TmCar {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                model: row.try_get("model")?,
                trim_badging: row.try_get("trim_badging")?,
                exterior_color: row.try_get("exterior_color")?,
                wheel_type: row.try_get("wheel_type")?,
                marketing_name: row.try_get("marketing_name")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .context("unexpected shape of TeslaMate's cars table")
}

/// The name of the day-number cursor in `sync_state` for a car's drives.
fn drive_job(car_id: i64) -> String {
    format!("teslamate_drive_id:{car_id}")
}

/// Works out one car's daily totals for the days that need it, in Postgres:
/// the local days of drives with an id above `after` or still going, and every
/// day from `recent_from` (YYYY-MM-DD) on. Also returns the car's highest drive
/// id, read before the totals so a drive added meanwhile is seen next run.
async fn fetch_drive_days(
    pg: &PgPool,
    car_id: i64,
    tz: Tz,
    after: i64,
    recent_from: &str,
) -> Result<(Vec<TmDriveDay>, Option<i64>)> {
    let newest: Option<i64> =
        sqlx::query_scalar("SELECT MAX(id)::bigint FROM drives WHERE car_id = $1")
            .bind(car_id)
            .fetch_one(pg)
            .await
            .context("could not read TeslaMate's newest drive")?;
    // The local day is taken from the start of the drive. The timestamps are
    // UTC without a zone: first say so, then convert to the local zone.
    let rows = sqlx::query(
        "WITH d AS (
           SELECT id, end_date, distance,
                  to_char((start_date AT TIME ZONE 'UTC' AT TIME ZONE $1)::date, 'YYYY-MM-DD') AS day
           FROM drives WHERE car_id = $2
         )
         SELECT day AS date, COALESCE(SUM(distance), 0)::float8 AS km, COUNT(*)::bigint AS drives
         FROM d
         WHERE day IN (SELECT day FROM d WHERE id > $3 OR end_date IS NULL) OR day >= $4
         GROUP BY day
         ORDER BY day",
    )
    .bind(tz.name())
    .bind(car_id)
    .bind(after)
    .bind(recent_from)
    .fetch_all(pg)
    .await
    .context("could not read TeslaMate's drives")?;
    let days = rows
        .iter()
        .map(|row| {
            Ok(TmDriveDay {
                car_id,
                date: row.try_get("date")?,
                km: row.try_get("km")?,
                drives: row.try_get("drives")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .context("unexpected shape of TeslaMate's drives table")?;
    Ok((days, newest))
}

/// Reads a car's charging sessions with an id above `newest` or in `open`.
async fn fetch_charges(
    pg: &PgPool,
    car_id: i64,
    newest: i64,
    open: &[i64],
) -> Result<Vec<TmCharge>> {
    let rows = sqlx::query(
        "SELECT cp.id::bigint AS id, cp.car_id::bigint AS car_id,
                EXTRACT(EPOCH FROM cp.start_date)::bigint AS started_at,
                EXTRACT(EPOCH FROM cp.end_date)::bigint AS ended_at,
                cp.charge_energy_added::float8 AS energy_added_kwh,
                cp.charge_energy_used::float8 AS energy_used_kwh,
                cp.start_battery_level::float8 AS start_soc_pct,
                cp.end_battery_level::float8 AS end_soc_pct,
                g.name AS geofence,
                COALESCE((SELECT bool_or(c.fast_charger_present) FROM charges c
                          WHERE c.charging_process_id = cp.id), false) AS fast_charger
         FROM charging_processes cp
         LEFT JOIN geofences g ON g.id = cp.geofence_id
         WHERE cp.car_id = $1 AND (cp.id > $2 OR cp.id = ANY($3))
         ORDER BY cp.id",
    )
    .bind(car_id)
    .bind(newest)
    .bind(open)
    .fetch_all(pg)
    .await
    .context("could not read TeslaMate's charging sessions")?;
    rows.iter()
        .map(|row| {
            Ok(TmCharge {
                id: row.try_get("id")?,
                car_id: row.try_get("car_id")?,
                started_at: row.try_get("started_at")?,
                ended_at: row.try_get("ended_at")?,
                energy_added_kwh: row.try_get("energy_added_kwh")?,
                energy_used_kwh: row.try_get("energy_used_kwh")?,
                start_soc_pct: row.try_get("start_soc_pct")?,
                end_soc_pct: row.try_get("end_soc_pct")?,
                geofence: row.try_get("geofence")?,
                fast_charger: row.try_get("fast_charger")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .context("unexpected shape of TeslaMate's charging_processes table")
}

/// The charger power during one charging session, read from TeslaMate when
/// needed instead of being copied: `(unix seconds, watts)`, about one a minute,
/// oldest first. Readings with no usable power are left out.
pub async fn charge_power(pg: &PgPool, charge_id: i64) -> Result<Vec<(i64, f64)>> {
    let rows = sqlx::query(
        "SELECT EXTRACT(EPOCH FROM date)::bigint AS ts,
                charger_power::float8 AS kw,
                charger_actual_current::float8 AS current,
                charger_voltage::float8 AS voltage,
                charger_phases::float8 AS phases
         FROM charges
         WHERE charging_process_id = $1
         ORDER BY date",
    )
    .bind(charge_id)
    .fetch_all(pg)
    .await
    .context("could not read TeslaMate's charge readings")?;
    let readings = rows
        .iter()
        .map(|row| {
            Ok(ChargeReading {
                ts: row.try_get("ts")?,
                charger_power_kw: row.try_get("kw")?,
                charger_actual_current: row.try_get("current")?,
                charger_voltage: row.try_get("voltage")?,
                charger_phases: row.try_get("phases")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .context("unexpected shape of TeslaMate's charges table")?;
    Ok(power_curve(&readings))
}

/// Costs the finished sessions at home that need it, at most
/// [`MAX_COSTED_PER_RUN`]. A failure on one session is logged and the rest go on.
async fn cost_sessions(
    pg: &PgPool,
    pool: &sqlx::SqlitePool,
    tz: Tz,
    setup: &CostSetup,
) -> Result<()> {
    let now = db::now();
    let finished = db::tm_charges_at_home(pool, now - COST_AFTER_SECS).await?;
    let todo = needing_cost(finished, setup, tz);
    let prices = ev_cost::session_prices(&setup.schedule, &setup.bills, tz);
    let mut costed = 0;
    for (session, key) in &todo {
        if costed >= MAX_COSTED_PER_RUN {
            break;
        }
        let outcome: Result<bool> = async {
            let (intervals, coverage) = web::house_intervals(
                pool,
                setup.plant_id,
                tz,
                session.started_at,
                session.ended_at,
                now,
            )
            .await?;
            if should_wait(coverage, session.ended_at, now) {
                return Ok(false);
            }
            let power = charge_power(pg, session.id).await?;
            let cost = ev_cost::historic_session_cost(
                &power,
                session.started_at,
                session.ended_at,
                session.energy_used_kwh,
                &intervals,
                &prices,
            );
            let stored = TmChargeCost {
                cost_key: key.clone(),
                grid_kwh: cost.grid_kwh,
                self_kwh: cost.self_kwh,
                uncovered_kwh: cost.uncovered_kwh,
                cost_cents: cost.cost_cents,
                baseline_cents: cost.baseline_cents,
                feed_in_forgone_cents: cost.feed_in_forgone_cents,
            };
            db::save_tm_charge_cost(pool, session.id, &stored).await?;
            Ok(true)
        }
        .await;
        match outcome {
            Ok(done) => costed += usize::from(done),
            Err(err) => warn!(
                charge = session.id,
                "could not cost a charging session: {err:#}"
            ),
        }
    }
    if costed > 0 {
        info!(
            costed,
            remaining = todo.len() - costed,
            "TeslaMate charging sessions costed"
        );
    }
    Ok(())
}

/// One run: copies what is new or unfinished from TeslaMate, then costs the
/// finished sessions at home if there is a tariff. Each table is written in
/// one transaction per car.
async fn run_once(
    pg: &PgPool,
    pool: &sqlx::SqlitePool,
    tz: Tz,
    costing: Option<&CostSetup>,
) -> Result<Counts> {
    let mut counts = Counts::default();

    let cars = fetch_cars(pg).await?;
    db::upsert_tm_cars(pool, &cars, db::now()).await?;
    counts.cars = cars.len();

    let today = Utc::now().with_timezone(&tz).date_naive();
    let recent_from = (today - Days::new(RECENT_DAYS - 1)).to_string();
    for car in &cars {
        let job = drive_job(car.id);
        let after = db::get_cursor(pool, &job).await?.unwrap_or(-1);
        let (days, newest) = fetch_drive_days(pg, car.id, tz, after, &recent_from).await?;
        db::upsert_tm_drive_days(pool, &days).await?;
        if let Some(newest) = newest {
            db::set_cursor(pool, &job, newest).await?;
        }
        counts.drive_days += days.len();

        let newest = db::max_tm_charge_id(pool, car.id).await?.unwrap_or(-1);
        let open = db::open_tm_charge_ids(pool, car.id).await?;
        let charges = fetch_charges(pg, car.id, newest, &open).await?;
        db::save_tm_charges(pool, &charges).await?;
        let updated = charges.iter().filter(|c| open.contains(&c.id)).count();
        counts.charges_updated += updated;
        counts.charges_new += charges.len() - updated;
    }
    if let Some(setup) = costing {
        cost_sessions(pg, pool, tz, setup).await?;
    }
    Ok(counts)
}

/// Connects, does one run and closes the connections again.
async fn connect_and_run(
    database_url: &str,
    pool: &sqlx::SqlitePool,
    tz: Tz,
    costing: Option<&CostSetup>,
) -> Result<Counts> {
    let pg = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(CONNECT_TIMEOUT)
        .connect(database_url)
        .await
        .context("could not connect to TeslaMate's database")?;
    let outcome = run_once(&pg, pool, tz, costing).await;
    pg.close().await;
    outcome
}

/// Runs the sync until `shutdown` completes. A failed run is logged and the
/// next one tries again. `tz` decides which local day a drive belongs to.
/// `costing` is `None` without a `[tariff]`, and sessions are then not costed.
pub async fn run(
    database_url: String,
    pool: sqlx::SqlitePool,
    tz: Tz,
    costing: Option<CostSetup>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    tokio::pin!(shutdown);
    info!("TeslaMate history sync started");
    let mut wait = STARTUP_DELAY;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(wait) => {}
        }
        let outcome = tokio::select! {
            _ = &mut shutdown => break,
            outcome = connect_and_run(&database_url, &pool, tz, costing.as_ref()) => outcome,
        };
        match outcome {
            Ok(c) => info!(
                cars = c.cars,
                drive_days = c.drive_days,
                charges_new = c.charges_new,
                charges_updated = c.charges_updated,
                "TeslaMate history synced"
            ),
            Err(err) => warn!("TeslaMate history sync failed: {err:#}; trying again later"),
        }
        wait = RUN_EVERY;
    }
    info!("TeslaMate history sync stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(
        kw: Option<f64>,
        current: Option<f64>,
        voltage: Option<f64>,
        phases: Option<f64>,
    ) -> ChargeReading {
        ChargeReading {
            ts: 1000,
            charger_power_kw: kw,
            charger_actual_current: current,
            charger_voltage: voltage,
            charger_phases: phases,
        }
    }

    #[test]
    fn power_prefers_current_times_voltage_times_phases() {
        let r = reading(Some(7.0), Some(32.0), Some(235.0), Some(1.0));
        assert_eq!(power_w(&r), Some(7520.0));
    }

    #[test]
    fn power_falls_back_to_rounded_kw() {
        // Any missing or zero part of the exact figure means the kW is used.
        for r in [
            reading(Some(7.0), None, Some(235.0), Some(1.0)),
            reading(Some(7.0), Some(32.0), None, Some(1.0)),
            reading(Some(7.0), Some(32.0), Some(235.0), None),
            reading(Some(7.0), Some(0.0), Some(235.0), Some(1.0)),
            reading(Some(7.0), Some(32.0), Some(0.0), Some(1.0)),
            reading(Some(7.0), Some(32.0), Some(235.0), Some(0.0)),
        ] {
            assert_eq!(power_w(&r), Some(7000.0), "{r:?}");
        }
        // A real zero is kept.
        assert_eq!(power_w(&reading(Some(0.0), None, None, None)), Some(0.0));
    }

    #[test]
    fn power_is_none_when_nothing_works() {
        assert_eq!(power_w(&reading(None, None, None, None)), None);
        assert_eq!(power_w(&reading(Some(-1.0), Some(0.0), None, None)), None);
        assert_eq!(power_w(&reading(Some(f64::NAN), None, None, None)), None);
    }

    #[test]
    fn curve_skips_readings_without_power() {
        let mut a = reading(Some(7.0), None, None, None);
        a.ts = 60;
        let mut b = reading(None, None, None, None);
        b.ts = 120;
        let mut c = reading(Some(3.0), None, None, None);
        c.ts = 180;
        assert_eq!(power_curve(&[a, b, c]), vec![(60, 7000.0), (180, 3000.0)]);
    }

    fn setup() -> CostSetup {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let w: Wrapper = toml::from_str(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 40.0\n\
             default_export_cents = 5.0\n",
        )
        .unwrap();
        CostSetup {
            schedule: std::sync::Arc::new(
                crate::cost::TariffSchedule::from_config(&w.tariff, &[]).unwrap(),
            ),
            bills: std::sync::Arc::from([]),
            plant_id: 1,
        }
    }

    fn session(id: i64, cost_key: Option<String>) -> TmChargeToCost {
        TmChargeToCost {
            id,
            started_at: 1_789_000_000,
            ended_at: 1_789_003_600,
            energy_used_kwh: Some(10.0),
            cost_key,
        }
    }

    #[test]
    fn only_sessions_with_a_stale_key_need_costing() {
        let setup = setup();
        let tz: Tz = "Australia/Adelaide".parse().unwrap();
        let key = ev_cost::historic_cost_key(&setup.schedule, &setup.bills, tz, 1_789_000_000);
        let todo = needing_cost(
            vec![
                session(1, None),
                session(2, Some(key.clone())),
                session(3, Some("old".to_string())),
            ],
            &setup,
            tz,
        );
        let ids: Vec<i64> = todo.iter().map(|(s, _)| s.id).collect();
        assert_eq!(ids, vec![1, 3]);
        assert!(todo.iter().all(|(_, k)| *k == key));
    }

    #[test]
    fn a_recent_session_with_little_house_data_waits() {
        let now = 10_000_000;
        let recent = now - 3600 * 5;
        let old = now - WAIT_FOR_DATA_SECS - 1;
        assert!(should_wait(0.5, recent, now));
        assert!(should_wait(0.979, recent, now));
        assert!(!should_wait(0.98, recent, now));
        assert!(!should_wait(1.0, recent, now));
        // After 3 days it is costed with what there is.
        assert!(!should_wait(0.0, old, now));
    }

    #[test]
    fn cars_query_selects_only_the_columns_that_exist() {
        let existing: Vec<String> = ["id", "name", "trim_badging", "wheel_type"]
            .map(String::from)
            .to_vec();
        let query = cars_query(&existing);
        assert!(query.contains("trim_badging::text AS trim_badging"));
        assert!(query.contains("wheel_type::text AS wheel_type"));
        // The ones this version lacks come back empty instead of failing.
        assert!(query.contains("NULL::text AS exterior_color"));
        assert!(query.contains("NULL::text AS marketing_name"));
        assert!(cars_query(&[]).contains("NULL::text AS wheel_type"));
    }

    #[test]
    fn drive_cursor_is_kept_per_car() {
        assert_ne!(drive_job(1), drive_job(10001));
    }
}
