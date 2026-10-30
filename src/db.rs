//! SQLite storage: opening the database, migrations and small typed queries.
//!
//! We use sqlx's runtime API (`sqlx::query`, `query_as`) instead of the
//! compile-time `query!` macros. The macros check SQL against a real database
//! while compiling, which needs DATABASE_URL or offline data; the runtime API
//! keeps builds simple at the cost of checking SQL only when it runs (the
//! tests below cover that).

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::cost::DayCost;
use crate::globird::data::DayRecord;
use crate::modbus::{Sample, Settings};

/// A measuring point (a kind of value, such as "battery power").
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub point_id: i64,
    pub code: Option<String>,
    pub name: Option<String>,
    pub unit: Option<String>,
}

/// One value of one point at one time.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub point_id: i64,
    /// Unix seconds, UTC.
    pub ts: i64,
    /// `None` when the API had no value for this time.
    pub value: Option<f64>,
}

/// Current time as unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_secs() as i64
}

/// Opens (creating if needed) the database file and runs the migrations.
pub async fn open(path: &str) -> Result<SqlitePool> {
    // Create the parent directory, e.g. `data/` for `data/solar.db`.
    if let Some(dir) = Path::new(path).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create directory {}", dir.display()))?;
    }

    // `filename` takes the path as-is, so spaces or `?` in it are fine.
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL lets readers (the web server) work while the sync task writes.
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    // A small box (1-2 cores): 4 connections is plenty, and WAL lets them read concurrently.
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .with_context(|| format!("could not open database {path}"))?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Applies the SQL files in `migrations/`, which are embedded in the binary.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!()
        .run(pool)
        .await
        .context("could not run database migrations")
}

pub async fn upsert_point(pool: &SqlitePool, point: &Point) -> Result<()> {
    sqlx::query(
        "INSERT INTO point (point_id, code, name, unit) VALUES (?, ?, ?, ?)
         ON CONFLICT (point_id) DO UPDATE
         SET code = excluded.code, name = excluded.name, unit = excluded.unit",
    )
    .bind(point.point_id)
    .bind(&point.code)
    .bind(&point.name)
    .bind(&point.unit)
    .execute(pool)
    .await
    .context("could not save point")?;
    Ok(())
}

/// Adds a point only if it has no row yet, so real metadata is never overwritten.
pub async fn insert_point_if_missing(pool: &SqlitePool, point: &Point) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO point (point_id, code, name, unit) VALUES (?, ?, ?, ?)")
        .bind(point.point_id)
        .bind(&point.code)
        .bind(&point.name)
        .bind(&point.unit)
        .execute(pool)
        .await
        .context("could not save point")?;
    Ok(())
}

/// Saves all readings in one transaction: either all of them are stored or none.
/// Saving the same reading again just overwrites its value.
pub async fn upsert_readings(pool: &SqlitePool, plant_id: i64, readings: &[Reading]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for r in readings {
        sqlx::query(
            "INSERT INTO reading (plant_id, point_id, ts, value) VALUES (?, ?, ?, ?)
             ON CONFLICT (plant_id, point_id, ts) DO UPDATE SET value = excluded.value",
        )
        .bind(plant_id)
        .bind(r.point_id)
        .bind(r.ts)
        .bind(r.value)
        .execute(&mut *tx)
        .await
        .context("could not save reading")?;
    }
    tx.commit().await?;
    Ok(())
}

/// How far a sync job has got, or `None` if it has never run.
pub async fn get_cursor(pool: &SqlitePool, job: &str) -> Result<Option<i64>> {
    let cursor = sqlx::query_scalar("SELECT cursor_ts FROM sync_state WHERE job = ?")
        .bind(job)
        .fetch_optional(pool)
        .await
        .context("could not read cursor")?;
    Ok(cursor)
}

pub async fn set_cursor(pool: &SqlitePool, job: &str, ts: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO sync_state (job, cursor_ts, updated_at) VALUES (?, ?, ?)
         ON CONFLICT (job) DO UPDATE
         SET cursor_ts = excluded.cursor_ts, updated_at = excluded.updated_at",
    )
    .bind(job)
    .bind(ts)
    .bind(now())
    .execute(pool)
    .await
    .context("could not save cursor")?;
    Ok(())
}

/// Logs one API call, used to stay under the API's rate limits.
pub async fn record_api_call(pool: &SqlitePool, endpoint: &str, ok: bool) -> Result<()> {
    sqlx::query("INSERT INTO api_call (endpoint, called_at, ok) VALUES (?, ?, ?)")
        .bind(endpoint)
        .bind(now())
        .bind(ok)
        .execute(pool)
        .await
        .context("could not record API call")?;
    Ok(())
}

/// How long `api_call` rows are kept. The budget looks back 30 days, so 40
/// leaves a margin.
const API_CALL_KEEP_SECS: i64 = 40 * 24 * 3600;

/// Deletes `api_call` rows too old to matter for the budget. Returns how many.
pub async fn prune_api_calls(pool: &SqlitePool) -> Result<u64> {
    let result = sqlx::query("DELETE FROM api_call WHERE called_at < ?")
        .bind(now() - API_CALL_KEEP_SECS)
        .execute(pool)
        .await
        .context("could not prune API calls")?;
    Ok(result.rows_affected())
}

/// Number of API calls made at or after `since_ts` (unix seconds).
pub async fn api_calls_since(pool: &SqlitePool, since_ts: i64) -> Result<i64> {
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM api_call WHERE called_at >= ?")
        .bind(since_ts)
        .fetch_one(pool)
        .await
        .context("could not count API calls")?;
    Ok(count)
}

/// The newest value of one point, with the point's name and unit.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct LatestReading {
    pub point_id: i64,
    pub name: Option<String>,
    pub unit: Option<String>,
    pub ts: i64,
    pub value: f64,
}

/// The newest timestamp of any reading in the plant, or `None` if there are none.
#[cfg(test)]
pub async fn max_reading_ts(pool: &SqlitePool, plant_id: i64) -> Result<Option<i64>> {
    // `MAX(ts) WHERE plant_id = ?` scans the whole plant, as the key is
    // (plant_id, point_id, ts). Instead take each point's newest ts from the
    // key, then the max of those. `MAX` gives NULL when there are none, hence `Option`.
    let ts = sqlx::query_scalar(
        "SELECT MAX((SELECT ts FROM reading r
                     WHERE r.plant_id = ?1 AND r.point_id = p.point_id
                     ORDER BY ts DESC LIMIT 1))
         FROM point p",
    )
    .bind(plant_id)
    .fetch_one(pool)
    .await
    .context("could not read newest timestamp")?;
    Ok(ts)
}

/// For each point, its most recent reading that has a value.
/// `CROSS JOIN` makes `point` the outer loop; otherwise SQLite scans the plant's readings.
pub async fn latest_readings(pool: &SqlitePool, plant_id: i64) -> Result<Vec<LatestReading>> {
    sqlx::query_as(
        "SELECT p.point_id, p.name, p.unit, r.ts, r.value
         FROM point p
         CROSS JOIN reading r ON r.plant_id = ?1 AND r.point_id = p.point_id
          AND r.ts = (SELECT ts FROM reading
                      WHERE plant_id = ?1 AND point_id = p.point_id
                        AND value IS NOT NULL
                      ORDER BY ts DESC LIMIT 1)
         ORDER BY p.point_id",
    )
    .bind(plant_id)
    .fetch_all(pool)
    .await
    .context("could not read latest readings")
}

/// Name and unit of a point, or `None` if the point is unknown.
pub async fn point_info(
    pool: &SqlitePool,
    point_id: i64,
) -> Result<Option<(Option<String>, Option<String>)>> {
    sqlx::query_as("SELECT name, unit FROM point WHERE point_id = ?")
        .bind(point_id)
        .fetch_optional(pool)
        .await
        .context("could not read point")
}

/// The `(ts, value)` pairs of one point with `from <= ts < to`, oldest first.
/// Rows without a value are left out.
pub async fn readings_in_range(
    pool: &SqlitePool,
    plant_id: i64,
    point_id: i64,
    from: i64,
    to: i64,
) -> Result<Vec<(i64, f64)>> {
    sqlx::query_as(
        "SELECT ts, value FROM reading
         WHERE plant_id = ? AND point_id = ? AND ts >= ? AND ts < ? AND value IS NOT NULL
         ORDER BY ts",
    )
    .bind(plant_id)
    .bind(point_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read readings")
}

/// The `(point_id, ts, value)` rows of several points with `from <= ts < to`,
/// ordered by point and then time. One query for all of them, so a long range
/// does not cost a query per day. Rows without a value are left out.
pub async fn readings_for_points(
    pool: &SqlitePool,
    plant_id: i64,
    point_ids: &[i64],
    from: i64,
    to: i64,
) -> Result<Vec<(i64, i64, f64)>> {
    // `QueryBuilder` adds the `?` for each point id, so no value is pasted into the SQL.
    let mut query =
        sqlx::QueryBuilder::new("SELECT point_id, ts, value FROM reading WHERE plant_id = ");
    query.push_bind(plant_id).push(" AND point_id IN (");
    let mut ids = query.separated(", ");
    for point_id in point_ids {
        ids.push_bind(point_id);
    }
    query
        .push(") AND ts >= ")
        .push_bind(from)
        .push(" AND ts < ")
        .push_bind(to)
        .push(" AND value IS NOT NULL ORDER BY point_id, ts");
    query
        .build_query_as()
        .fetch_all(pool)
        .await
        .context("could not read readings")
}

/// The largest value of one point with `from <= ts < to`, or `None` if there is none.
pub async fn max_value_in_range(
    pool: &SqlitePool,
    plant_id: i64,
    point_id: i64,
    from: i64,
    to: i64,
) -> Result<Option<f64>> {
    let max = sqlx::query_scalar(
        "SELECT MAX(value) FROM reading
         WHERE plant_id = ? AND point_id = ? AND ts >= ? AND ts < ?",
    )
    .bind(plant_id)
    .bind(point_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await
    .context("could not read maximum")?;
    Ok(max)
}

/// Saves one live Modbus sample. A second sample with the same second
/// replaces the first.
pub async fn insert_live_sample(pool: &SqlitePool, sample: &Sample) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO live_sample (ts, pv_w, load_w, grid_w, battery_w, soc_pct)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(sample.ts)
    .bind(sample.pv_w)
    .bind(sample.load_w)
    .bind(sample.grid_w)
    .bind(sample.battery_w)
    .bind(sample.soc_pct)
    .execute(pool)
    .await
    .context("could not save live sample")?;
    Ok(())
}

/// The newest live sample, or `None` if there are none.
pub async fn latest_live_sample(pool: &SqlitePool) -> Result<Option<Sample>> {
    sqlx::query_as(
        "SELECT ts, pv_w, load_w, grid_w, battery_w, soc_pct
         FROM live_sample ORDER BY ts DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .context("could not read latest live sample")
}

/// The highest grid import (W) among the live samples from `since` on, as
/// `(ts, import_w)`, or `None` if nothing was imported in that time.
///
/// `live_sample` is keyed by `ts`, so this is a range scan over one day of
/// rows (about 5,800 at one sample per 15 s), which SQLite does in about a
/// millisecond. That is cheap enough to run on every `/api/live` poll, and
/// unlike a running maximum kept in memory it survives a restart.
pub async fn peak_grid_import(pool: &SqlitePool, since: i64) -> Result<Option<(i64, f64)>> {
    sqlx::query_as(
        "SELECT ts, grid_w FROM live_sample
         WHERE ts >= ? AND grid_w > 0
         ORDER BY grid_w DESC, ts LIMIT 1",
    )
    .bind(since)
    .fetch_optional(pool)
    .await
    .context("could not read the peak grid import")
}

/// Saves the inverter's lifetime grid totals (Wh) as read at `ts`. A second
/// row with the same second replaces the first.
pub async fn insert_grid_meter(
    pool: &SqlitePool,
    ts: i64,
    import_wh: f64,
    export_wh: f64,
) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO grid_meter (ts, import_wh, export_wh) VALUES (?, ?, ?)")
        .bind(ts)
        .bind(import_wh)
        .bind(export_wh)
        .execute(pool)
        .await
        .context("could not save grid totals")?;
    Ok(())
}

/// The newest grid totals as `(ts, import_wh, export_wh)`, or `None` if there
/// are none.
pub async fn latest_grid_meter(pool: &SqlitePool) -> Result<Option<(i64, f64, f64)>> {
    sqlx::query_as("SELECT ts, import_wh, export_wh FROM grid_meter ORDER BY ts DESC LIMIT 1")
        .fetch_optional(pool)
        .await
        .context("could not read latest grid totals")
}

/// Every grid totals row with `from <= ts < to`, oldest first, as
/// `(ts, import_wh, export_wh)`.
pub async fn grid_meter_raw(pool: &SqlitePool, from: i64, to: i64) -> Result<Vec<(i64, f64, f64)>> {
    sqlx::query_as(
        "SELECT ts, import_wh, export_wh FROM grid_meter WHERE ts >= ? AND ts < ? ORDER BY ts",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read grid totals")
}

/// Saves one row of inverter settings. A second row with the same second
/// replaces the first.
pub async fn insert_inverter_settings(pool: &SqlitePool, s: &Settings) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO inverter_settings (ts, ems_mode, forced_command, forced_power_w,
                max_soc_pct, min_soc_pct, export_limit_w, export_limit_enabled,
                backup_reserve_pct, max_charge_w, max_discharge_w, running_state)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(s.ts)
    .bind(s.ems_mode)
    .bind(s.forced_command)
    .bind(s.forced_power_w)
    .bind(s.max_soc_pct)
    .bind(s.min_soc_pct)
    .bind(s.export_limit_w)
    .bind(s.export_limit_enabled)
    .bind(s.backup_reserve_pct)
    .bind(s.max_charge_w)
    .bind(s.max_discharge_w)
    .bind(s.running_state)
    .execute(pool)
    .await
    .context("could not save inverter settings")?;
    Ok(())
}

/// The newest saved inverter settings, or `None` if there are none.
pub async fn latest_inverter_settings(pool: &SqlitePool) -> Result<Option<Settings>> {
    sqlx::query_as(
        "SELECT ts, ems_mode, forced_command, forced_power_w, max_soc_pct,
                min_soc_pct, export_limit_w, export_limit_enabled, backup_reserve_pct,
                max_charge_w, max_discharge_w, running_state
         FROM inverter_settings ORDER BY ts DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .context("could not read latest inverter settings")
}

/// Saved inverter settings with `from <= ts < to`, newest first, at most `limit`.
pub async fn inverter_settings_in_range(
    pool: &SqlitePool,
    from: i64,
    to: i64,
    limit: i64,
) -> Result<Vec<Settings>> {
    sqlx::query_as(
        "SELECT ts, ems_mode, forced_command, forced_power_w, max_soc_pct,
                min_soc_pct, export_limit_w, export_limit_enabled, backup_reserve_pct,
                max_charge_w, max_discharge_w, running_state
         FROM inverter_settings
         WHERE ts >= ? AND ts < ? ORDER BY ts DESC LIMIT ?",
    )
    .bind(from)
    .bind(to)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("could not read inverter settings")
}

/// The newest saved inverter settings with `ts < before`, or `None`.
pub async fn inverter_settings_before(pool: &SqlitePool, before: i64) -> Result<Option<Settings>> {
    sqlx::query_as(
        "SELECT ts, ems_mode, forced_command, forced_power_w, max_soc_pct,
                min_soc_pct, export_limit_w, export_limit_enabled, backup_reserve_pct,
                max_charge_w, max_discharge_w, running_state
         FROM inverter_settings WHERE ts < ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(before)
    .fetch_optional(pool)
    .await
    .context("could not read inverter settings")
}

/// Live samples with `from <= ts < to`, averaged into 1-minute buckets,
/// oldest first. Each returned `ts` is the start of its minute. A column
/// that is NULL for the whole minute stays `None`.
pub async fn live_samples_in_range(pool: &SqlitePool, from: i64, to: i64) -> Result<Vec<Sample>> {
    // `ts / 60 * 60` is integer division, so it rounds down to the minute.
    // `AVG` skips NULLs.
    sqlx::query_as(
        "SELECT ts / 60 * 60 AS ts,
                AVG(pv_w) AS pv_w, AVG(load_w) AS load_w, AVG(grid_w) AS grid_w,
                AVG(battery_w) AS battery_w, AVG(soc_pct) AS soc_pct
         FROM live_sample
         WHERE ts >= ? AND ts < ?
         GROUP BY ts / 60 * 60
         ORDER BY 1",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read live samples")
}

/// The oldest and newest live sample times, or `None` if there are none.
pub async fn live_ts_range(pool: &SqlitePool) -> Result<Option<(i64, i64)>> {
    // With no rows `MIN` and `MAX` are both NULL, so read them as `Option`s.
    // Separate subqueries let SQLite use the key for each; `MIN(ts), MAX(ts)`
    // together scan the whole table.
    let (min, max): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT MIN(ts) FROM live_sample), (SELECT MAX(ts) FROM live_sample)",
    )
    .fetch_one(pool)
    .await
    .context("could not read live sample range")?;
    Ok(min.zip(max))
}

/// The oldest reading of any of `point_ids`, or `None` if there is none.
pub async fn first_reading_ts(
    pool: &SqlitePool,
    plant_id: i64,
    point_ids: &[i64],
) -> Result<Option<i64>> {
    let mut first: Option<i64> = None;
    // One lookup per point, each answered from the (plant, point, ts) key.
    for &point_id in point_ids {
        let ts: Option<i64> =
            sqlx::query_scalar("SELECT MIN(ts) FROM reading WHERE plant_id = ? AND point_id = ?")
                .bind(plant_id)
                .bind(point_id)
                .fetch_one(pool)
                .await
                .context("could not read the first reading")?;
        first = match (first, ts) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    Ok(first)
}

/// Every live sample with `from <= ts < to`, oldest first, not averaged.
pub async fn live_samples_raw(pool: &SqlitePool, from: i64, to: i64) -> Result<Vec<Sample>> {
    sqlx::query_as(
        "SELECT ts, pv_w, load_w, grid_w, battery_w, soc_pct
         FROM live_sample WHERE ts >= ? AND ts < ? ORDER BY ts",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read live samples")
}

/// A finished local day's cost as stored in `daily_cost`.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedDayCost {
    /// Local day, `YYYY-MM-DD`.
    pub date: String,
    /// "inverter" or "cloud".
    pub source: String,
    pub cost: DayCost,
}

/// `daily_cost` columns as sqlx reads them; [`CachedDayCost`] is the tidy form.
#[derive(sqlx::FromRow)]
struct DailyCostRow {
    date: String,
    source: String,
    import_cents: f64,
    export_cents: f64,
    supply_cents: f64,
    credit_cents: f64,
    actual_cents: f64,
    baseline_cents: f64,
    savings_cents: f64,
    import_kwh: f64,
    export_kwh: f64,
    load_kwh: f64,
}

impl From<DailyCostRow> for CachedDayCost {
    fn from(row: DailyCostRow) -> Self {
        Self {
            date: row.date,
            source: row.source,
            cost: DayCost {
                import_kwh: row.import_kwh,
                export_kwh: row.export_kwh,
                load_kwh: row.load_kwh,
                import_cents: row.import_cents,
                export_cents: row.export_cents,
                supply_cents: row.supply_cents,
                zero_import_credit_cents: row.credit_cents,
                actual_cents: row.actual_cents,
                baseline_cents: row.baseline_cents,
                savings_cents: row.savings_cents,
            },
        }
    }
}

/// Stored day costs with `from <= date <= to` (`YYYY-MM-DD` text sorts like
/// dates) that were worked out with `tariff_key`. Rows made with other rates
/// are skipped, so they count as missing and get replaced.
pub async fn daily_costs_in_range(
    pool: &SqlitePool,
    from: &str,
    to: &str,
    tariff_key: &str,
) -> Result<Vec<CachedDayCost>> {
    let rows: Vec<DailyCostRow> = sqlx::query_as(
        "SELECT date, source, import_cents, export_cents, supply_cents, credit_cents,
                actual_cents, baseline_cents, savings_cents, import_kwh, export_kwh, load_kwh
         FROM daily_cost WHERE date >= ? AND date <= ? AND tariff_key = ? ORDER BY date",
    )
    .bind(from)
    .bind(to)
    .bind(tariff_key)
    .fetch_all(pool)
    .await
    .context("could not read stored day costs")?;
    Ok(rows.into_iter().map(CachedDayCost::from).collect())
}

/// The average daily load (kWh) per calendar month (1 to 12, any year) over
/// the stored day costs. One small grouped query on the `daily_cost` table;
/// load does not depend on the tariff, so every stored row counts. A month
/// with no stored day is missing from the map.
pub async fn average_load_by_month(pool: &SqlitePool) -> Result<HashMap<u32, f64>> {
    let rows: Vec<(i64, f64)> = sqlx::query_as(
        "SELECT CAST(substr(date, 6, 2) AS INTEGER), AVG(load_kwh) FROM daily_cost GROUP BY 1",
    )
    .fetch_all(pool)
    .await
    .context("could not read the average daily load")?;
    Ok(rows
        .into_iter()
        .map(|(month, kwh)| (month as u32, kwh))
        .collect())
}

/// Saves finished days' costs (replacing any row for the same date), all in
/// one transaction so a long backfill is one disk sync, not one per day.
pub async fn save_daily_costs(
    pool: &SqlitePool,
    days: &[CachedDayCost],
    tariff_key: &str,
) -> Result<()> {
    let computed_at = now();
    let mut tx = pool.begin().await?;
    for day in days {
        let c = &day.cost;
        sqlx::query(
            "INSERT OR REPLACE INTO daily_cost
               (date, source, import_cents, export_cents, supply_cents, credit_cents,
                actual_cents, baseline_cents, savings_cents, import_kwh, export_kwh, load_kwh,
                tariff_key, computed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&day.date)
        .bind(&day.source)
        .bind(c.import_cents)
        .bind(c.export_cents)
        .bind(c.supply_cents)
        .bind(c.zero_import_credit_cents)
        .bind(c.actual_cents)
        .bind(c.baseline_cents)
        .bind(c.savings_cents)
        .bind(c.import_kwh)
        .bind(c.export_kwh)
        .bind(c.load_kwh)
        .bind(tariff_key)
        .bind(computed_at)
        .execute(&mut *tx)
        .await
        .context("could not save day cost")?;
    }
    tx.commit().await?;
    Ok(())
}

/// What we know about a charging session so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChargeProgress {
    /// Energy that reached the car's battery, kWh.
    pub energy_added_kwh: Option<f64>,
    /// Energy drawn from the wall, kWh. More than `energy_added_kwh`: the
    /// difference is charging losses.
    pub energy_used_kwh: Option<f64>,
    /// The latest battery level, 0 to 100.
    pub soc_pct: Option<f64>,
    pub max_power_kw: Option<f64>,
}

/// A session that has started but not ended, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenCharge {
    pub id: i64,
    pub progress: ChargeProgress,
}

/// Starts a session and returns its id. If the car already has an open
/// session (the table allows only one), nothing is added and that
/// session's id is returned.
pub async fn start_charge_session(
    pool: &SqlitePool,
    car_id: i64,
    started_at: i64,
    start_soc_pct: Option<f64>,
    geofence: Option<&str>,
) -> Result<i64> {
    sqlx::query(
        "INSERT OR IGNORE INTO ev_charge_session (car_id, started_at, start_soc_pct, geofence)
         VALUES (?, ?, ?, ?)",
    )
    .bind(car_id)
    .bind(started_at)
    .bind(start_soc_pct)
    .bind(geofence)
    .execute(pool)
    .await
    .context("could not start charge session")?;
    // Either the row just added or the one that was already open.
    sqlx::query_scalar("SELECT id FROM ev_charge_session WHERE car_id = ? AND ended_at IS NULL")
        .bind(car_id)
        .fetch_one(pool)
        .await
        .context("could not read charge session")
}

/// `ev_charge_session` columns as sqlx reads them for [`OpenCharge`].
#[derive(sqlx::FromRow)]
struct OpenChargeRow {
    id: i64,
    energy_added_kwh: Option<f64>,
    energy_used_kwh: Option<f64>,
    end_soc_pct: Option<f64>,
    max_power_kw: Option<f64>,
}

/// The car's open session, or `None` if it is not charging.
pub async fn open_charge_session(pool: &SqlitePool, car_id: i64) -> Result<Option<OpenCharge>> {
    let row: Option<OpenChargeRow> = sqlx::query_as(
        "SELECT id, energy_added_kwh, energy_used_kwh, end_soc_pct, max_power_kw
         FROM ev_charge_session WHERE car_id = ? AND ended_at IS NULL",
    )
    .bind(car_id)
    .fetch_optional(pool)
    .await
    .context("could not read open charge session")?;
    Ok(row.map(|row| OpenCharge {
        id: row.id,
        progress: ChargeProgress {
            energy_added_kwh: row.energy_added_kwh,
            energy_used_kwh: row.energy_used_kwh,
            soc_pct: row.end_soc_pct,
            max_power_kw: row.max_power_kw,
        },
    }))
}

/// Saves a running session's latest values. A value we do not have keeps
/// what is stored.
pub async fn update_charge_session(
    pool: &SqlitePool,
    id: i64,
    progress: &ChargeProgress,
) -> Result<()> {
    sqlx::query(
        "UPDATE ev_charge_session
         SET energy_added_kwh = COALESCE(?, energy_added_kwh),
             energy_used_kwh = COALESCE(?, energy_used_kwh),
             end_soc_pct = COALESCE(?, end_soc_pct),
             max_power_kw = COALESCE(?, max_power_kw)
         WHERE id = ?",
    )
    .bind(progress.energy_added_kwh)
    .bind(progress.energy_used_kwh)
    .bind(progress.soc_pct)
    .bind(progress.max_power_kw)
    .bind(id)
    .execute(pool)
    .await
    .context("could not update charge session")?;
    Ok(())
}

/// Saves the final values and closes the session.
pub async fn end_charge_session(
    pool: &SqlitePool,
    id: i64,
    ended_at: i64,
    progress: &ChargeProgress,
) -> Result<()> {
    update_charge_session(pool, id, progress).await?;
    sqlx::query("UPDATE ev_charge_session SET ended_at = ? WHERE id = ?")
        .bind(ended_at)
        .bind(id)
        .execute(pool)
        .await
        .context("could not end charge session")?;
    Ok(())
}

/// A charging session as sent by `/api/ev/sessions`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow, serde::Serialize)]
pub struct ChargeSession {
    pub id: i64,
    pub started_at: i64,
    /// `None` while the car is still charging.
    pub ended_at: Option<i64>,
    pub energy_added_kwh: Option<f64>,
    pub energy_used_kwh: Option<f64>,
    pub start_soc_pct: Option<f64>,
    pub end_soc_pct: Option<f64>,
    pub max_power_kw: Option<f64>,
    pub geofence: Option<String>,
}

/// Sessions that overlap `from..=to`, oldest first. A session that has not
/// ended counts as running until `now`.
pub async fn charge_sessions_in_range(
    pool: &SqlitePool,
    from: i64,
    to: i64,
    now: i64,
) -> Result<Vec<ChargeSession>> {
    sqlx::query_as(
        "SELECT id, started_at, ended_at, energy_added_kwh, energy_used_kwh, start_soc_pct,
                end_soc_pct, max_power_kw, geofence
         FROM ev_charge_session
         WHERE started_at <= ? AND COALESCE(ended_at, ?) >= ?
         ORDER BY started_at",
    )
    .bind(to)
    .bind(now)
    .bind(from)
    .fetch_all(pool)
    .await
    .context("could not read charge sessions")
}

/// Saves the odometer reading (km) for a local day, replacing the day's
/// earlier one.
pub async fn save_odometer(
    pool: &SqlitePool,
    date: &str,
    odometer_km: f64,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO ev_odometer_daily (date, odometer_km, updated_at) VALUES (?, ?, ?)
         ON CONFLICT (date) DO UPDATE SET odometer_km = excluded.odometer_km,
                                          updated_at = excluded.updated_at",
    )
    .bind(date)
    .bind(odometer_km)
    .bind(now)
    .execute(pool)
    .await
    .context("could not save odometer")?;
    Ok(())
}

/// The odometer rows (local date, km) from `from` to `to`, oldest first, plus
/// the last row before `from`, so the first day's km can be worked out.
pub async fn odometer_in_range(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<Vec<(String, f64)>> {
    sqlx::query_as(
        "SELECT date, odometer_km FROM ev_odometer_daily
         WHERE date >= COALESCE((SELECT MAX(date) FROM ev_odometer_daily WHERE date < ?), ?)
           AND date <= ?
         ORDER BY date",
    )
    .bind(from)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read odometer")
}

/// Replaces everything stored for one GloBird day with `day`, in one
/// transaction: the day's old rows are deleted first, so a line GloBird has
/// since revised or removed does not linger. Either all of it is saved or none.
pub async fn save_globird_day(pool: &SqlitePool, day: &DayRecord, fetched_at: i64) -> Result<()> {
    let date = day.date.to_string(); // "2026-10-09"
    let mut tx = pool.begin().await?;
    for delete in [
        "DELETE FROM globird_cost WHERE date = ?",
        "DELETE FROM globird_usage WHERE date = ?",
        "DELETE FROM globird_interval WHERE date = ?",
    ] {
        sqlx::query(delete)
            .bind(&date)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("could not clear old GloBird rows for {date}"))?;
    }
    for line in &day.cost {
        sqlx::query(
            "INSERT INTO globird_cost
               (date, category, amount_cents, quantity, meter_read_missing, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&date)
        .bind(&line.category)
        .bind(line.amount_cents)
        .bind(line.quantity)
        .bind(line.meter_read_missing)
        .bind(fetched_at)
        .execute(&mut *tx)
        .await
        .context("could not save GloBird cost")?;
    }
    for line in &day.usage {
        sqlx::query(
            "INSERT INTO globird_usage
               (date, suffix, charge_type, category_code, kwh, quality, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&date)
        .bind(&line.suffix)
        .bind(&line.charge_type)
        .bind(&line.category_code)
        .bind(line.kwh)
        .bind(&line.quality)
        .bind(fetched_at)
        .execute(&mut *tx)
        .await
        .context("could not save GloBird usage")?;
    }
    for series in &day.intervals {
        let intervals = serde_json::to_string(&series.intervals)
            .context("could not write GloBird intervals as JSON")?;
        sqlx::query(
            "INSERT INTO globird_interval
               (date, suffix, direction, kwh_total, intervals, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&date)
        .bind(&series.suffix)
        .bind(series.direction.as_str())
        .bind(series.kwh_total)
        .bind(intervals)
        .bind(fetched_at)
        .execute(&mut *tx)
        .await
        .context("could not save GloBird intervals")?;
    }
    tx.commit().await?;
    Ok(())
}

/// The newest stored GloBird day that is complete, i.e. has a cost row other
/// than SUPPLY. (`globird::data::is_supply` is the same rule in Rust.)
pub async fn latest_complete_globird_date(pool: &SqlitePool) -> Result<Option<NaiveDate>> {
    let date: Option<String> = sqlx::query_scalar(
        "SELECT MAX(date) FROM globird_cost WHERE UPPER(TRIM(category)) <> 'SUPPLY'",
    )
    .fetch_one(pool)
    .await
    .context("could not read the newest complete GloBird day")?;
    date.map(|text| {
        NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .with_context(|| format!("bad date {text:?} in globird_cost"))
    })
    .transpose()
}

/// Whether a stored GloBird day is complete (see [`latest_complete_globird_date`]).
pub async fn globird_day_is_complete(pool: &SqlitePool, date: NaiveDate) -> Result<bool> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM globird_cost
         WHERE date = ? AND UPPER(TRIM(category)) <> 'SUPPLY' LIMIT 1",
    )
    .bind(date.to_string())
    .fetch_optional(pool)
    .await
    .context("could not check a GloBird day")?;
    Ok(found.is_some())
}

/// One stored GloBird cost line (`globird_cost`), as the compare API reads it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct GlobirdCostRow {
    /// Local day, `YYYY-MM-DD`.
    pub date: String,
    /// GloBird's own text, e.g. "USAGE".
    pub category: String,
    /// Cents including GST; negative is a credit.
    pub amount_cents: i64,
    pub quantity: f64,
}

/// Every GloBird cost line for the local days `from` to `to` (`YYYY-MM-DD`,
/// both included), oldest day first.
pub async fn globird_cost_in_range(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<Vec<GlobirdCostRow>> {
    sqlx::query_as(
        "SELECT date, category, amount_cents, quantity FROM globird_cost
         WHERE date >= ? AND date <= ? ORDER BY date, category",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read GloBird costs")
}

/// The kWh GloBird metered on one day in one direction (`import` or `export`).
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct GlobirdDayKwh {
    pub date: String,
    pub direction: String,
    pub kwh: f64,
}

/// The metered kWh per day and direction for the local days `from` to `to`.
/// The total of the 5-minute `globird_interval` readings is used when the day
/// has it for that direction. Otherwise the kWh of the `globird_usage` charge
/// periods are added up (a register starting with E is import, B is export).
pub async fn globird_kwh_in_range(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<Vec<GlobirdDayKwh>> {
    sqlx::query_as(
        "SELECT date, direction, SUM(kwh_total) AS kwh FROM globird_interval
         WHERE date >= ?1 AND date <= ?2 GROUP BY date, direction
         UNION ALL
         SELECT u.date,
                CASE UPPER(SUBSTR(TRIM(u.suffix), 1, 1)) WHEN 'E' THEN 'import' ELSE 'export' END,
                SUM(u.kwh)
         FROM globird_usage u
         WHERE u.date >= ?1 AND u.date <= ?2
           AND UPPER(SUBSTR(TRIM(u.suffix), 1, 1)) IN ('E', 'B')
           AND NOT EXISTS (
             SELECT 1 FROM globird_interval i
             WHERE i.date = u.date
               AND i.direction = CASE UPPER(SUBSTR(TRIM(u.suffix), 1, 1))
                                   WHEN 'E' THEN 'import' ELSE 'export' END)
         GROUP BY u.date, 2
         ORDER BY 1, 2",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("could not read GloBird kWh")
}

/// One car from TeslaMate, as stored in `tm_car`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TmCar {
    pub id: i64,
    pub name: Option<String>,
    pub model: Option<String>,
    /// TeslaMate's `trim_badging`, such as "Long Range".
    pub trim_badging: Option<String>,
    /// TeslaMate's `exterior_color`, the paint.
    pub exterior_color: Option<String>,
    pub wheel_type: Option<String>,
    pub marketing_name: Option<String>,
}

/// One car's driving on one local day, as stored in `tm_drive_daily`.
#[derive(Debug, Clone, PartialEq)]
pub struct TmDriveDay {
    pub car_id: i64,
    /// The local day, YYYY-MM-DD.
    pub date: String,
    pub km: f64,
    pub drives: i64,
}

/// One charging session from TeslaMate, as stored in `tm_charge`. Times are
/// unix seconds, UTC.
#[derive(Debug, Clone, PartialEq)]
pub struct TmCharge {
    pub id: i64,
    pub car_id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub energy_added_kwh: Option<f64>,
    pub energy_used_kwh: Option<f64>,
    pub start_soc_pct: Option<f64>,
    pub end_soc_pct: Option<f64>,
    pub geofence: Option<String>,
    pub fast_charger: bool,
}

/// Saves TeslaMate's cars, replacing the ones already stored, in one transaction.
pub async fn upsert_tm_cars(pool: &SqlitePool, cars: &[TmCar], now: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    for car in cars {
        sqlx::query(
            "INSERT INTO tm_car (id, name, model, trim_badging, exterior_color, wheel_type,
                                 marketing_name, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, model = excluded.model,
               trim_badging = excluded.trim_badging, exterior_color = excluded.exterior_color,
               wheel_type = excluded.wheel_type, marketing_name = excluded.marketing_name,
               updated_at = excluded.updated_at",
        )
        .bind(car.id)
        .bind(&car.name)
        .bind(&car.model)
        .bind(&car.trim_badging)
        .bind(&car.exterior_color)
        .bind(&car.wheel_type)
        .bind(&car.marketing_name)
        .bind(now)
        .execute(&mut *tx)
        .await
        .context("could not save a TeslaMate car")?;
    }
    tx.commit().await?;
    Ok(())
}

/// Saves daily driving totals, replacing the stored ones for the same car
/// and day, in one transaction.
pub async fn upsert_tm_drive_days(pool: &SqlitePool, days: &[TmDriveDay]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for d in days {
        sqlx::query(
            "INSERT INTO tm_drive_daily (car_id, date, km, drives) VALUES (?, ?, ?, ?)
             ON CONFLICT (car_id, date) DO UPDATE SET km = excluded.km, drives = excluded.drives",
        )
        .bind(d.car_id)
        .bind(&d.date)
        .bind(d.km)
        .bind(d.drives)
        .execute(&mut *tx)
        .await
        .context("could not save TeslaMate daily driving")?;
    }
    tx.commit().await?;
    Ok(())
}

/// The highest charging session id stored for a car (see [`max_tm_drive_id`]).
pub async fn max_tm_charge_id(pool: &SqlitePool, car_id: i64) -> Result<Option<i64>> {
    sqlx::query_scalar("SELECT MAX(id) FROM tm_charge WHERE car_id = ?")
        .bind(car_id)
        .fetch_one(pool)
        .await
        .context("could not read the newest stored charge")
}

/// The ids of a car's stored charging sessions that had not ended when they
/// were last read.
pub async fn open_tm_charge_ids(pool: &SqlitePool, car_id: i64) -> Result<Vec<i64>> {
    sqlx::query_scalar("SELECT id FROM tm_charge WHERE car_id = ? AND ended_at IS NULL")
        .bind(car_id)
        .fetch_all(pool)
        .await
        .context("could not read the open charges")
}

/// Saves charging sessions, replacing any stored with the same id, in one
/// transaction.
pub async fn save_tm_charges(pool: &SqlitePool, charges: &[TmCharge]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for c in charges {
        sqlx::query(
            "INSERT INTO tm_charge (id, car_id, started_at, ended_at, energy_added_kwh,
                                    energy_used_kwh, start_soc_pct, end_soc_pct, geofence,
                                    fast_charger)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET car_id = excluded.car_id,
               started_at = excluded.started_at, ended_at = excluded.ended_at,
               energy_added_kwh = excluded.energy_added_kwh,
               energy_used_kwh = excluded.energy_used_kwh,
               start_soc_pct = excluded.start_soc_pct, end_soc_pct = excluded.end_soc_pct,
               geofence = excluded.geofence, fast_charger = excluded.fast_charger",
        )
        .bind(c.id)
        .bind(c.car_id)
        .bind(c.started_at)
        .bind(c.ended_at)
        .bind(c.energy_added_kwh)
        .bind(c.energy_used_kwh)
        .bind(c.start_soc_pct)
        .bind(c.end_soc_pct)
        .bind(&c.geofence)
        .bind(c.fast_charger)
        .execute(&mut *tx)
        .await
        .context("could not save a TeslaMate charge")?;
    }
    tx.commit().await?;
    Ok(())
}

/// A finished session at home, as far as costing it needs.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TmChargeToCost {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub energy_used_kwh: Option<f64>,
    /// What it was costed with, `None` if it was not.
    pub cost_key: Option<String>,
}

/// What a session cost, as stored on its `tm_charge` row (see
/// `ev_cost::HistoricCost`).
#[derive(Debug, Clone, PartialEq)]
pub struct TmChargeCost {
    pub cost_key: String,
    pub grid_kwh: f64,
    pub self_kwh: f64,
    pub uncovered_kwh: f64,
    pub cost_cents: f64,
    pub baseline_cents: f64,
    pub feed_in_forgone_cents: f64,
}

/// The sessions at home (geofence "Home") that ended at or before
/// `ended_by`, newest first, with the key they were costed with. The caller
/// picks those whose key is out of date, because the right key depends on the
/// session's day.
pub async fn tm_charges_at_home(pool: &SqlitePool, ended_by: i64) -> Result<Vec<TmChargeToCost>> {
    sqlx::query_as(
        "SELECT id, started_at, ended_at, energy_used_kwh, cost_key FROM tm_charge
         WHERE geofence = 'Home' AND ended_at IS NOT NULL AND ended_at <= ?
         ORDER BY started_at DESC",
    )
    .bind(ended_by)
    .fetch_all(pool)
    .await
    .context("could not read the charges at home")
}

/// Stores what a session cost on its row.
pub async fn save_tm_charge_cost(pool: &SqlitePool, id: i64, cost: &TmChargeCost) -> Result<()> {
    sqlx::query(
        "UPDATE tm_charge SET cost_key = ?, grid_kwh = ?, self_kwh = ?, uncovered_kwh = ?,
                              cost_cents = ?, baseline_cents = ?, feed_in_forgone_cents = ?
         WHERE id = ?",
    )
    .bind(&cost.cost_key)
    .bind(cost.grid_kwh)
    .bind(cost.self_kwh)
    .bind(cost.uncovered_kwh)
    .bind(cost.cost_cents)
    .bind(cost.baseline_cents)
    .bind(cost.feed_in_forgone_cents)
    .bind(id)
    .execute(pool)
    .await
    .context("could not save a charge's cost")?;
    Ok(())
}

/// Every car in `tm_car`, by id.
pub async fn tm_cars(pool: &SqlitePool) -> Result<Vec<TmCar>> {
    type Row = (
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, name, model, trim_badging, exterior_color, wheel_type, marketing_name
         FROM tm_car ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("could not read the TeslaMate cars")?;
    Ok(rows
        .into_iter()
        .map(
            |(id, name, model, trim_badging, exterior_color, wheel_type, marketing_name)| TmCar {
                id,
                name,
                model,
                trim_badging,
                exterior_color,
                wheel_type,
                marketing_name,
            },
        )
        .collect())
}

/// A car's km on each local day it drove, oldest first.
pub async fn tm_drive_days(pool: &SqlitePool, car_id: i64) -> Result<Vec<TmDriveDay>> {
    let rows: Vec<(String, f64, i64)> = sqlx::query_as(
        "SELECT date, km, drives FROM tm_drive_daily WHERE car_id = ? ORDER BY date",
    )
    .bind(car_id)
    .fetch_all(pool)
    .await
    .context("could not read a car's daily km")?;
    Ok(rows
        .into_iter()
        .map(|(date, km, drives)| TmDriveDay {
            car_id,
            date,
            km,
            drives,
        })
        .collect())
}

/// A charging session with what it cost, as far as the savings summary needs.
/// The cost columns are `None` until the session has been costed.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TmChargeRow {
    pub started_at: i64,
    pub energy_added_kwh: Option<f64>,
    pub energy_used_kwh: Option<f64>,
    pub geofence: Option<String>,
    pub cost_key: Option<String>,
    pub grid_kwh: Option<f64>,
    pub self_kwh: Option<f64>,
    pub uncovered_kwh: Option<f64>,
    pub cost_cents: Option<f64>,
    pub baseline_cents: Option<f64>,
    pub feed_in_forgone_cents: Option<f64>,
}

/// A car's charging sessions, oldest first.
pub async fn tm_charge_rows(pool: &SqlitePool, car_id: i64) -> Result<Vec<TmChargeRow>> {
    sqlx::query_as(
        "SELECT started_at, energy_added_kwh, energy_used_kwh, geofence, cost_key, grid_kwh,
                self_kwh, uncovered_kwh, cost_cents, baseline_cents, feed_in_forgone_cents
         FROM tm_charge WHERE car_id = ? ORDER BY started_at, id",
    )
    .bind(car_id)
    .fetch_all(pool)
    .await
    .context("could not read a car's charges")
}

/// A car TeslaMate does not track, as stored in `manual_car`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ManualCar {
    pub id: i64,
    pub name: String,
    pub model: Option<String>,
    /// `phev`, `petrol`, `hybrid` or `diesel`; only shown, never used in sums.
    pub kind: String,
    /// Unix seconds, UTC.
    pub created_at: i64,
}

/// One petrol fill, as stored in `fuel_fill`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct FuelFill {
    pub id: i64,
    pub car_id: i64,
    /// The local day, YYYY-MM-DD.
    pub date: String,
    pub litres: f64,
    pub cost_dollars: f64,
    pub odometer_km: Option<f64>,
    /// Whether the tank was filled to full (needed to measure fuel economy).
    pub full_tank: bool,
    pub note: String,
}

/// Every manual car, by id.
pub async fn manual_cars(pool: &SqlitePool) -> Result<Vec<ManualCar>> {
    sqlx::query_as("SELECT id, name, model, kind, created_at FROM manual_car ORDER BY id")
        .fetch_all(pool)
        .await
        .context("could not read the manual cars")
}

/// Every fill of every manual car, by car and then oldest first.
pub async fn fuel_fills(pool: &SqlitePool) -> Result<Vec<FuelFill>> {
    sqlx::query_as(
        "SELECT id, car_id, date, litres, cost_dollars, odometer_km, full_tank, note
         FROM fuel_fill ORDER BY car_id, date, id",
    )
    .fetch_all(pool)
    .await
    .context("could not read the fuel fills")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh database in a temp directory. Keep the `TempDir` alive for as
    /// long as the pool is used: dropping it deletes the files.
    async fn test_db() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        // A nested path also checks that missing directories are created.
        let path = dir.path().join("sub/test.db");
        let pool = open(path.to_str().unwrap()).await.unwrap();
        (dir, pool)
    }

    fn point(point_id: i64) -> Point {
        Point {
            point_id,
            code: Some("p13003".to_string()),
            name: Some("Power".to_string()),
            unit: Some("W".to_string()),
        }
    }

    fn reading(point_id: i64, ts: i64, value: f64) -> Reading {
        Reading {
            point_id,
            ts,
            value: Some(value),
        }
    }

    #[tokio::test]
    async fn migrations_create_tables() {
        let (_dir, pool) = test_db().await;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN
             ('point', 'reading', 'sync_state', 'api_call', 'daily_cost', 'ev_charge_session',
              'ev_odometer_daily', 'globird_cost', 'globird_usage', 'globird_interval',
              'tm_car', 'tm_drive_daily', 'tm_charge', 'manual_car', 'fuel_fill')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tables.len(), 15);
    }

    #[tokio::test]
    async fn charge_session_runs_from_start_to_end() {
        let (_dir, pool) = test_db().await;
        assert_eq!(open_charge_session(&pool, 1).await.unwrap(), None);

        let id = start_charge_session(&pool, 1, 1000, Some(40.0), Some("Home"))
            .await
            .unwrap();
        let open = open_charge_session(&pool, 1).await.unwrap().unwrap();
        assert_eq!(open.id, id);
        assert_eq!(open.progress, ChargeProgress::default());

        let progress = ChargeProgress {
            energy_added_kwh: Some(3.5),
            energy_used_kwh: Some(3.9),
            soc_pct: Some(45.0),
            max_power_kw: Some(7.4),
        };
        update_charge_session(&pool, id, &progress).await.unwrap();
        assert_eq!(
            open_charge_session(&pool, 1)
                .await
                .unwrap()
                .unwrap()
                .progress,
            progress
        );

        // Unknown values keep what is stored.
        let later = ChargeProgress {
            soc_pct: Some(50.0),
            ..ChargeProgress::default()
        };
        end_charge_session(&pool, id, 5000, &later).await.unwrap();
        assert_eq!(open_charge_session(&pool, 1).await.unwrap(), None);

        let rows = charge_sessions_in_range(&pool, 0, 10_000, 6000)
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![ChargeSession {
                id,
                started_at: 1000,
                ended_at: Some(5000),
                energy_added_kwh: Some(3.5),
                energy_used_kwh: Some(3.9),
                start_soc_pct: Some(40.0),
                end_soc_pct: Some(50.0),
                max_power_kw: Some(7.4),
                geofence: Some("Home".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn a_car_never_gets_two_open_sessions() {
        let (_dir, pool) = test_db().await;
        let first = start_charge_session(&pool, 1, 1000, None, None)
            .await
            .unwrap();
        // Starting again hands back the open one instead of adding a row.
        let again = start_charge_session(&pool, 1, 2000, None, None)
            .await
            .unwrap();
        assert_eq!(first, again);
        // Another car is separate.
        let other = start_charge_session(&pool, 2, 2000, None, None)
            .await
            .unwrap();
        assert_ne!(first, other);

        // Once closed, a new session can start.
        end_charge_session(&pool, first, 3000, &ChargeProgress::default())
            .await
            .unwrap();
        let next = start_charge_session(&pool, 1, 4000, None, None)
            .await
            .unwrap();
        assert_ne!(first, next);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ev_charge_session")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn charge_sessions_in_range_picks_the_overlapping_ones() {
        let (_dir, pool) = test_db().await;
        // 100..200 (done), 300..400 (done), and one still open from 500.
        for (car, start, end) in [(1, 100, Some(200)), (1, 300, Some(400)), (1, 500, None)] {
            let id = start_charge_session(&pool, car, start, None, None)
                .await
                .unwrap();
            if let Some(end) = end {
                end_charge_session(&pool, id, end, &ChargeProgress::default())
                    .await
                    .unwrap();
            }
        }
        let starts =
            |rows: Vec<ChargeSession>| -> Vec<i64> { rows.iter().map(|r| r.started_at).collect() };
        let all = charge_sessions_in_range(&pool, 0, 1000, 600).await.unwrap();
        assert_eq!(starts(all), vec![100, 300, 500]);
        // Touching a session's end or start counts as overlapping.
        let some = charge_sessions_in_range(&pool, 200, 300, 600)
            .await
            .unwrap();
        assert_eq!(starts(some), vec![100, 300]);
        let gap = charge_sessions_in_range(&pool, 201, 299, 600)
            .await
            .unwrap();
        assert!(gap.is_empty());
        // The open session runs until `now`: it is there at 550 but not before it started.
        let open = charge_sessions_in_range(&pool, 550, 560, 600)
            .await
            .unwrap();
        assert_eq!(starts(open), vec![500]);
        let early = charge_sessions_in_range(&pool, 450, 460, 600)
            .await
            .unwrap();
        assert!(early.is_empty());
    }

    #[tokio::test]
    async fn upsert_readings_is_idempotent_and_updates() {
        let (_dir, pool) = test_db().await;
        upsert_point(&pool, &point(1)).await.unwrap();

        let first = [reading(1, 100, 1.5), reading(1, 200, 2.5)];
        upsert_readings(&pool, 7, &first).await.unwrap();
        upsert_readings(&pool, 7, &first).await.unwrap();

        // Same key (plant 7, point 1, ts 100) with a new value.
        upsert_readings(&pool, 7, &[reading(1, 100, 9.0)])
            .await
            .unwrap();

        let rows: Vec<(i64, Option<f64>)> =
            sqlx::query_as("SELECT ts, value FROM reading ORDER BY ts")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![(100, Some(9.0)), (200, Some(2.5))]);
    }

    #[tokio::test]
    async fn insert_point_if_missing_keeps_existing_row() {
        let (_dir, pool) = test_db().await;
        let real = Point {
            name: Some("Real name".to_string()),
            ..point(1)
        };
        upsert_point(&pool, &real).await.unwrap();
        let placeholder = Point {
            name: None,
            ..point(1)
        };
        insert_point_if_missing(&pool, &placeholder).await.unwrap();
        insert_point_if_missing(&pool, &point(2)).await.unwrap();

        let names: Vec<Option<String>> =
            sqlx::query_scalar("SELECT name FROM point ORDER BY point_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].as_deref(), Some("Real name"));
    }

    #[tokio::test]
    async fn reading_needs_a_known_point() {
        let (_dir, pool) = test_db().await;
        // Foreign keys are on, so point 99 must exist first.
        let result = upsert_readings(&pool, 7, &[reading(99, 100, 1.0)]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cursor_round_trips() {
        let (_dir, pool) = test_db().await;
        assert_eq!(get_cursor(&pool, "backfill").await.unwrap(), None);
        set_cursor(&pool, "backfill", 1000).await.unwrap();
        set_cursor(&pool, "backfill", 2000).await.unwrap();
        assert_eq!(get_cursor(&pool, "backfill").await.unwrap(), Some(2000));
        assert_eq!(get_cursor(&pool, "other").await.unwrap(), None);
    }

    #[tokio::test]
    async fn api_calls_since_counts_recent_calls() {
        let (_dir, pool) = test_db().await;
        record_api_call(&pool, "/a", true).await.unwrap();
        record_api_call(&pool, "/b", false).await.unwrap();
        assert_eq!(api_calls_since(&pool, now() - 60).await.unwrap(), 2);
        assert_eq!(api_calls_since(&pool, now() + 60).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn prune_api_calls_removes_only_old_rows() {
        let (_dir, pool) = test_db().await;
        let old = now() - (API_CALL_KEEP_SECS + 60);
        sqlx::query("INSERT INTO api_call (endpoint, called_at, ok) VALUES ('/old', ?, 1)")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        record_api_call(&pool, "/new", true).await.unwrap();
        assert_eq!(prune_api_calls(&pool).await.unwrap(), 1);
        assert_eq!(api_calls_since(&pool, 0).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn raw_response_table_is_dropped() {
        let (_dir, pool) = test_db().await;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'raw_response'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn range_queries_read_back_readings() {
        let (_dir, pool) = test_db().await;
        upsert_point(&pool, &point(1)).await.unwrap();
        let null_reading = Reading {
            point_id: 1,
            ts: 300,
            value: None,
        };
        let rows = [reading(1, 100, 1.0), reading(1, 200, 5.0), null_reading];
        upsert_readings(&pool, 7, &rows).await.unwrap();

        assert_eq!(max_reading_ts(&pool, 7).await.unwrap(), Some(300));
        assert_eq!(max_reading_ts(&pool, 8).await.unwrap(), None);
        // The end is exclusive and NULL values are skipped.
        let got = readings_in_range(&pool, 7, 1, 100, 300).await.unwrap();
        assert_eq!(got, vec![(100, 1.0), (200, 5.0)]);
        assert_eq!(
            max_value_in_range(&pool, 7, 1, 0, 1000).await.unwrap(),
            Some(5.0)
        );
        assert_eq!(max_value_in_range(&pool, 7, 1, 0, 100).await.unwrap(), None);

        // The newest reading that has a value is 200, not the NULL at 300.
        let latest = latest_readings(&pool, 7).await.unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!((latest[0].ts, latest[0].value), (200, 5.0));
        assert_eq!(latest[0].name.as_deref(), Some("Power"));
    }

    #[tokio::test]
    async fn newest_queries_span_several_points() {
        let (_dir, pool) = test_db().await;
        for id in 1..=3 {
            upsert_point(&pool, &point(id)).await.unwrap();
        }
        let null_reading = Reading {
            point_id: 2,
            ts: 500,
            value: None,
        };
        let rows = [
            reading(1, 100, 1.0),
            reading(1, 400, 2.0),
            reading(2, 200, 3.0),
            null_reading,
            reading(3, 300, 4.0),
        ];
        upsert_readings(&pool, 7, &rows).await.unwrap();
        // Another plant's newer reading must not leak in.
        upsert_readings(&pool, 8, &[reading(1, 900, 9.0)])
            .await
            .unwrap();

        // The newest ts is on point 2 (a NULL value), not the last point.
        assert_eq!(max_reading_ts(&pool, 7).await.unwrap(), Some(500));
        let latest = latest_readings(&pool, 7).await.unwrap();
        let got: Vec<_> = latest.iter().map(|l| (l.point_id, l.ts, l.value)).collect();
        assert_eq!(got, vec![(1, 400, 2.0), (2, 200, 3.0), (3, 300, 4.0)]);
    }

    fn sample(ts: i64, pv_w: f64) -> Sample {
        Sample {
            ts,
            pv_w: Some(pv_w),
            load_w: Some(700.0),
            grid_w: Some(-50.0),
            battery_w: Some(200.0),
            soc_pct: Some(43.4),
        }
    }

    #[tokio::test]
    async fn peak_grid_import_is_the_highest_import_since_a_time() {
        let (_dir, pool) = test_db().await;
        assert_eq!(peak_grid_import(&pool, 0).await.unwrap(), None);

        let grid = |ts, grid_w| Sample {
            grid_w,
            ..sample(ts, 0.0)
        };
        for s in [
            grid(100, Some(20_000.0)), // before `since`
            grid(200, Some(9_000.0)),
            grid(215, Some(14_600.0)),
            grid(230, Some(14_600.0)), // a tie keeps the earlier time
            grid(245, Some(-5_000.0)), // exporting is never the peak
            grid(260, None),
        ] {
            insert_live_sample(&pool, &s).await.unwrap();
        }
        assert_eq!(
            peak_grid_import(&pool, 150).await.unwrap(),
            Some((215, 14_600.0))
        );
        // Only exporting since then: no peak.
        assert_eq!(peak_grid_import(&pool, 240).await.unwrap(), None);
    }

    #[tokio::test]
    async fn live_samples_round_trip() {
        let (_dir, pool) = test_db().await;
        assert_eq!(latest_live_sample(&pool).await.unwrap(), None);

        insert_live_sample(&pool, &sample(100, 1.0)).await.unwrap();
        insert_live_sample(&pool, &sample(115, 2.0)).await.unwrap();
        // The same second again replaces the row.
        insert_live_sample(&pool, &sample(115, 3.0)).await.unwrap();
        // NULLs are kept as NULLs.
        let unavailable = Sample {
            pv_w: None,
            ..sample(130, 0.0)
        };
        insert_live_sample(&pool, &unavailable).await.unwrap();

        assert_eq!(latest_live_sample(&pool).await.unwrap(), Some(unavailable));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_sample")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn grid_meter_round_trip() {
        let (_dir, pool) = test_db().await;
        assert_eq!(latest_grid_meter(&pool).await.unwrap(), None);

        insert_grid_meter(&pool, 100, 1000.0, 2000.0).await.unwrap();
        insert_grid_meter(&pool, 200, 1100.0, 2000.0).await.unwrap();
        // The same second again replaces the row.
        insert_grid_meter(&pool, 200, 1100.0, 2100.0).await.unwrap();
        insert_grid_meter(&pool, 300, 1100.0, 2200.0).await.unwrap();

        assert_eq!(
            latest_grid_meter(&pool).await.unwrap(),
            Some((300, 1100.0, 2200.0))
        );
        // `to` is exclusive, `from` inclusive, oldest first.
        assert_eq!(
            grid_meter_raw(&pool, 100, 300).await.unwrap(),
            vec![(100, 1000.0, 2000.0), (200, 1100.0, 2100.0)]
        );
    }

    fn settings(ts: i64, ems_mode: u16) -> Settings {
        Settings {
            ts,
            ems_mode: Some(ems_mode),
            forced_command: Some(0xAA),
            forced_power_w: Some(8000),
            max_soc_pct: Some(100.0),
            min_soc_pct: Some(5.5),
            export_limit_w: None,
            export_limit_enabled: Some(true),
            backup_reserve_pct: Some(10),
            max_charge_w: Some(8000),
            max_discharge_w: None,
            running_state: Some(0x8200),
        }
    }

    #[tokio::test]
    async fn inverter_settings_round_trip() {
        let (_dir, pool) = test_db().await;
        assert_eq!(latest_inverter_settings(&pool).await.unwrap(), None);

        insert_inverter_settings(&pool, &settings(100, 0))
            .await
            .unwrap();
        insert_inverter_settings(&pool, &settings(200, 2))
            .await
            .unwrap();
        // NULLs and false are kept as they are.
        let blank = Settings {
            ems_mode: None,
            export_limit_enabled: Some(false),
            ..settings(300, 0)
        };
        insert_inverter_settings(&pool, &blank).await.unwrap();

        assert_eq!(latest_inverter_settings(&pool).await.unwrap(), Some(blank));
    }

    #[tokio::test]
    async fn inverter_settings_in_range_are_newest_first() {
        let (_dir, pool) = test_db().await;
        for ts in [100, 200, 300, 400] {
            insert_inverter_settings(&pool, &settings(ts, 0))
                .await
                .unwrap();
        }
        // `to` is exclusive.
        let rows = inverter_settings_in_range(&pool, 200, 400, 10)
            .await
            .unwrap();
        let times: Vec<i64> = rows.iter().map(|r| r.ts).collect();
        assert_eq!(times, [300, 200]);
        // The limit keeps the newest rows.
        let rows = inverter_settings_in_range(&pool, 0, 1000, 3).await.unwrap();
        let times: Vec<i64> = rows.iter().map(|r| r.ts).collect();
        assert_eq!(times, [400, 300, 200]);
        assert!(
            inverter_settings_in_range(&pool, 500, 600, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inverter_settings_before_is_strictly_earlier() {
        let (_dir, pool) = test_db().await;
        assert_eq!(inverter_settings_before(&pool, 1000).await.unwrap(), None);
        for ts in [100, 200, 300] {
            insert_inverter_settings(&pool, &settings(ts, 0))
                .await
                .unwrap();
        }
        // A row at exactly `before` does not count.
        let row = inverter_settings_before(&pool, 300).await.unwrap();
        assert_eq!(row.map(|r| r.ts), Some(200));
        let row = inverter_settings_before(&pool, 301).await.unwrap();
        assert_eq!(row.map(|r| r.ts), Some(300));
        assert_eq!(inverter_settings_before(&pool, 100).await.unwrap(), None);
    }

    #[tokio::test]
    async fn live_samples_are_averaged_per_minute() {
        let (_dir, pool) = test_db().await;
        // Minute 120: three samples, one with no PV value. Minute 180: one.
        // 240 is outside the range.
        let rows = [
            sample(125, 100.0),
            sample(140, 300.0),
            Sample {
                pv_w: None,
                load_w: Some(900.0),
                ..sample(170, 0.0)
            },
            sample(180, 50.0),
            sample(240, 9.0),
        ];
        for row in &rows {
            insert_live_sample(&pool, row).await.unwrap();
        }

        let got = live_samples_in_range(&pool, 100, 240).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts, 120);
        // AVG ignores the NULL: (100 + 300) / 2.
        assert_eq!(got[0].pv_w, Some(200.0));
        assert_eq!(got[0].load_w, Some((700.0 + 700.0 + 900.0) / 3.0));
        assert_eq!(got[1], sample(180, 50.0));

        // Nothing was stored before 100.
        assert!(
            live_samples_in_range(&pool, 0, 100)
                .await
                .unwrap()
                .is_empty()
        );
        // A minute with only NULL PV values keeps PV as None.
        let got = live_samples_in_range(&pool, 170, 171).await.unwrap();
        assert_eq!(got[0].pv_w, None);
    }

    #[tokio::test]
    async fn live_range_and_raw_samples() {
        let (_dir, pool) = test_db().await;
        assert_eq!(live_ts_range(&pool).await.unwrap(), None);
        for ts in [100, 115, 130] {
            insert_live_sample(&pool, &sample(ts, 1.0)).await.unwrap();
        }
        assert_eq!(live_ts_range(&pool).await.unwrap(), Some((100, 130)));
        // Not averaged: all three rows come back, and the end is exclusive.
        assert_eq!(live_samples_raw(&pool, 100, 130).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn day_costs_round_trip_and_follow_the_tariff_key() {
        let (_dir, pool) = test_db().await;
        let cost = DayCost {
            import_kwh: 1.5,
            export_kwh: 2.5,
            load_kwh: 4.0,
            import_cents: 10.0,
            export_cents: 20.0,
            supply_cents: 100.0,
            zero_import_credit_cents: 5.0,
            actual_cents: 85.0,
            baseline_cents: 200.0,
            savings_cents: 115.0,
        };
        let day = |date: &str| CachedDayCost {
            date: date.to_string(),
            source: "cloud".to_string(),
            cost: cost.clone(),
        };
        save_daily_costs(&pool, &[day("2026-09-14"), day("2026-09-15")], "a")
            .await
            .unwrap();

        let found = daily_costs_in_range(&pool, "2026-09-15", "2026-09-30", "a")
            .await
            .unwrap();
        assert_eq!(found, vec![day("2026-09-15")]);
        // Another tariff key sees nothing, and saving again replaces the row.
        let other = daily_costs_in_range(&pool, "2026-09-01", "2026-09-30", "b")
            .await
            .unwrap();
        assert!(other.is_empty());
        save_daily_costs(&pool, &[day("2026-09-15")], "b")
            .await
            .unwrap();
        let both = daily_costs_in_range(&pool, "2026-09-01", "2026-09-30", "a")
            .await
            .unwrap();
        assert_eq!(both, vec![day("2026-09-14")]);
    }

    #[tokio::test]
    async fn odometer_keeps_the_latest_reading_of_each_day() {
        let (_dir, pool) = test_db().await;
        save_odometer(&pool, "2026-10-01", 1000.0, 1).await.unwrap();
        save_odometer(&pool, "2026-10-03", 1050.5, 2).await.unwrap();
        save_odometer(&pool, "2026-10-03", 1060.0, 3).await.unwrap();
        save_odometer(&pool, "2026-10-05", 1100.0, 4).await.unwrap();
        let dates = |rows: Vec<(String, f64)>| -> Vec<String> {
            rows.into_iter().map(|(date, _)| date).collect()
        };
        // From the 3rd: the row before it comes along as the starting point.
        let got = odometer_in_range(&pool, "2026-10-03", "2026-10-31")
            .await
            .unwrap();
        assert_eq!(got[1], ("2026-10-03".to_string(), 1060.0));
        assert_eq!(dates(got), vec!["2026-10-01", "2026-10-03", "2026-10-05"]);
        // A day with no row of its own still finds the one before it.
        let got = odometer_in_range(&pool, "2026-10-04", "2026-10-04")
            .await
            .unwrap();
        assert_eq!(dates(got), vec!["2026-10-03"]);
        // Nothing before the first row, and the end is respected.
        let got = odometer_in_range(&pool, "2026-09-01", "2026-10-02")
            .await
            .unwrap();
        assert_eq!(dates(got), vec!["2026-10-01"]);
    }

    /// Row counts of the three GloBird tables for one day.
    async fn globird_counts(pool: &SqlitePool, date: &str) -> (i64, i64, i64) {
        let mut counts = Vec::new();
        for sql in [
            "SELECT COUNT(*) FROM globird_cost WHERE date = ?",
            "SELECT COUNT(*) FROM globird_usage WHERE date = ?",
            "SELECT COUNT(*) FROM globird_interval WHERE date = ?",
        ] {
            let n = sqlx::query_scalar(sql)
                .bind(date)
                .fetch_one(pool)
                .await
                .unwrap();
            counts.push(n);
        }
        (counts[0], counts[1], counts[2])
    }

    #[tokio::test]
    async fn globird_kwh_prefers_intervals_and_falls_back_to_usage() {
        use crate::globird::data::{group_days, parse_cost_rows, parse_usage_rows};
        let (_dir, pool) = test_db().await;
        let data_of = |text: &str| -> serde_json::Value {
            let mut reply: serde_json::Value = serde_json::from_str(text).unwrap();
            reply["data"].take()
        };
        let cost =
            parse_cost_rows(data_of(include_str!("globird/testdata/cost_detail.json"))).unwrap();
        let usage = parse_usage_rows(data_of(include_str!(
            "globird/testdata/smart_meter_read.json"
        )))
        .unwrap();
        let days = group_days(&cost, &usage);
        for day in &days {
            save_globird_day(&pool, day, 1000).await.unwrap();
        }
        let kwh = globird_kwh_in_range(&pool, "2026-10-09", "2026-10-09")
            .await
            .unwrap();
        let totals: Vec<(&str, f64)> = days[0]
            .intervals
            .iter()
            .map(|s| (s.direction.as_str(), s.kwh_total))
            .collect();
        assert_eq!(kwh.len(), 2);
        for row in &kwh {
            let expected = totals.iter().find(|(d, _)| *d == row.direction).unwrap().1;
            assert!((row.kwh - expected).abs() < 1e-9, "{row:?}");
        }

        // Without the interval rows the charge-period kWh are added up.
        sqlx::query("DELETE FROM globird_interval WHERE date = '2026-10-09'")
            .execute(&pool)
            .await
            .unwrap();
        let fallback = globird_kwh_in_range(&pool, "2026-10-09", "2026-10-09")
            .await
            .unwrap();
        assert_eq!(fallback.len(), 2);
        let import: f64 = days[0]
            .usage
            .iter()
            .filter(|u| u.suffix.starts_with('E'))
            .map(|u| u.kwh)
            .sum();
        let got = fallback.iter().find(|r| r.direction == "import").unwrap();
        assert!((got.kwh - import).abs() < 1e-9);
    }

    #[tokio::test]
    async fn globird_day_is_replaced_not_merged() {
        use crate::globird::data::{group_days, parse_cost_rows, parse_usage_rows};
        let (_dir, pool) = test_db().await;

        // Two real days, as the sync would write them.
        let data_of = |text: &str| -> serde_json::Value {
            let mut reply: serde_json::Value = serde_json::from_str(text).unwrap();
            reply["data"].take()
        };
        let cost =
            parse_cost_rows(data_of(include_str!("globird/testdata/cost_detail.json"))).unwrap();
        let usage = parse_usage_rows(data_of(include_str!(
            "globird/testdata/smart_meter_read.json"
        )))
        .unwrap();
        let days = group_days(&cost, &usage);
        for day in &days {
            save_globird_day(&pool, day, 1000).await.unwrap();
        }
        assert_eq!(globird_counts(&pool, "2026-10-09").await, (6, 8, 2));
        assert_eq!(globird_counts(&pool, "2026-10-17").await, (4, 7, 2));

        // Spot-check the values: cents, an unchanged array and the direction.
        let (cents, missing): (i64, bool) = sqlx::query_as(
            "SELECT amount_cents, meter_read_missing FROM globird_cost
             WHERE date = '2026-10-09' AND category = 'ZEROHERO Credit'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((cents, missing), (-100, false));
        let (direction, json): (String, String) = sqlx::query_as(
            "SELECT direction, intervals FROM globird_interval
             WHERE date = '2026-10-09' AND suffix = 'B7'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(direction, "export");
        let stored: Vec<f64> = serde_json::from_str(&json).unwrap();
        assert_eq!(stored, days[0].intervals[0].intervals);
        assert_eq!(
            latest_complete_globird_date(&pool).await.unwrap(),
            NaiveDate::from_ymd_opt(2026, 10, 17)
        );

        // GloBird revises the 9th: fewer lines. The old ones must be gone.
        let mut revised = days[0].clone();
        revised.cost.truncate(2);
        revised.usage.truncate(1);
        revised.intervals.clear();
        save_globird_day(&pool, &revised, 2000).await.unwrap();
        assert_eq!(globird_counts(&pool, "2026-10-09").await, (2, 1, 0));
        // The other day is untouched.
        assert_eq!(globird_counts(&pool, "2026-10-17").await, (4, 7, 2));
        let fetched: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT fetched_at FROM globird_cost WHERE date = '2026-10-09'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(fetched, [2000]);
    }

    #[tokio::test]
    async fn globird_completeness_ignores_supply() {
        use crate::globird::data::{CostRow, group_days};
        let (_dir, pool) = test_db().await;
        let date = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        assert_eq!(latest_complete_globird_date(&pool).await.unwrap(), None);

        let row = |d: u32, category: &str| CostRow {
            date: date(d),
            category: category.to_string(),
            amount_cents: 100,
            quantity: 0.0,
            meter_read_missing: false,
        };
        // The 20th is complete; the 21st has only SUPPLY so far.
        let rows = [row(20, "SUPPLY"), row(20, "USAGE"), row(21, "SUPPLY")];
        for day in group_days(&rows, &[]) {
            save_globird_day(&pool, &day, 1).await.unwrap();
        }
        assert_eq!(
            latest_complete_globird_date(&pool).await.unwrap(),
            Some(date(20))
        );
        assert!(globird_day_is_complete(&pool, date(20)).await.unwrap());
        assert!(!globird_day_is_complete(&pool, date(21)).await.unwrap());
        assert!(!globird_day_is_complete(&pool, date(22)).await.unwrap());
    }

    fn tm_charge(id: i64, car_id: i64, ended_at: Option<i64>) -> TmCharge {
        TmCharge {
            id,
            car_id,
            started_at: 2000 + id,
            ended_at,
            energy_added_kwh: Some(10.0),
            energy_used_kwh: Some(11.0),
            start_soc_pct: Some(20.0),
            end_soc_pct: Some(40.0),
            geofence: Some("Home".to_string()),
            fast_charger: false,
        }
    }

    #[tokio::test]
    async fn tm_cars_are_replaced_by_id() {
        let (_dir, pool) = test_db().await;
        let car = |name: &str| TmCar {
            id: 10001,
            name: Some(name.to_string()),
            model: Some("3".to_string()),
            trim_badging: Some("Long Range".to_string()),
            ..TmCar::default()
        };
        upsert_tm_cars(&pool, &[car("Old")], 5).await.unwrap();
        upsert_tm_cars(&pool, &[car("Renamed")], 9).await.unwrap();
        let rows: Vec<(i64, Option<String>, i64)> =
            sqlx::query_as("SELECT id, name, updated_at FROM tm_car")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![(10001, Some("Renamed".to_string()), 9)]);
        let read = tm_cars(&pool).await.unwrap();
        assert_eq!(read[0].trim_badging.as_deref(), Some("Long Range"));
        assert_eq!(read[0].wheel_type, None);
    }

    #[tokio::test]
    async fn tm_drive_days_are_replaced_per_car_and_day() {
        let (_dir, pool) = test_db().await;
        let day = |car_id: i64, date: &str, km: f64, drives: i64| TmDriveDay {
            car_id,
            date: date.to_string(),
            km,
            drives,
        };
        upsert_tm_drive_days(
            &pool,
            &[day(1, "2026-10-20", 5.0, 1), day(2, "2026-10-20", 9.0, 2)],
        )
        .await
        .unwrap();
        // The same day again, after another drive.
        upsert_tm_drive_days(&pool, &[day(1, "2026-10-20", 12.5, 2)])
            .await
            .unwrap();
        let rows: Vec<(i64, String, f64, i64)> =
            sqlx::query_as("SELECT car_id, date, km, drives FROM tm_drive_daily ORDER BY car_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "2026-10-20".to_string(), 12.5, 2),
                (2, "2026-10-20".to_string(), 9.0, 2)
            ]
        );
    }

    #[tokio::test]
    async fn tm_charges_track_newest_and_open_per_car() {
        let (_dir, pool) = test_db().await;
        assert_eq!(max_tm_charge_id(&pool, 1).await.unwrap(), None);
        // The old car's ids are far above the new car's.
        save_tm_charges(
            &pool,
            &[
                tm_charge(100_000_005, 10001, Some(9)),
                tm_charge(3, 1, None),
                tm_charge(4, 1, Some(9)),
            ],
        )
        .await
        .unwrap();
        assert_eq!(max_tm_charge_id(&pool, 1).await.unwrap(), Some(4));
        assert_eq!(open_tm_charge_ids(&pool, 1).await.unwrap(), vec![3]);

        // The open session ends and is saved again in place.
        let mut done = tm_charge(3, 1, Some(300));
        done.fast_charger = true;
        save_tm_charges(&pool, &[done]).await.unwrap();
        assert!(open_tm_charge_ids(&pool, 1).await.unwrap().is_empty());
        let (count, fast): (i64, bool) =
            sqlx::query_as("SELECT COUNT(*), MAX(fast_charger) FROM tm_charge WHERE car_id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((count, fast), (2, true));
    }

    #[tokio::test]
    async fn tm_charge_costs_are_saved_on_the_row() {
        let (_dir, pool) = test_db().await;
        let mut away = tm_charge(2, 1, Some(100));
        away.geofence = Some("Work".to_string());
        let mut nowhere = tm_charge(3, 1, Some(100));
        nowhere.geofence = None;
        save_tm_charges(
            &pool,
            &[
                tm_charge(1, 1, Some(100)),
                away,
                nowhere,
                tm_charge(4, 1, None),
                tm_charge(5, 1, Some(900)),
            ],
        )
        .await
        .unwrap();
        // Only finished sessions at Home that ended in time, newest first.
        let todo = tm_charges_at_home(&pool, 500).await.unwrap();
        assert_eq!(todo.len(), 1);
        assert_eq!((todo[0].id, todo[0].ended_at), (1, 100));
        assert_eq!(todo[0].energy_used_kwh, Some(11.0));
        assert_eq!(todo[0].cost_key, None);

        let cost = TmChargeCost {
            cost_key: "k1".to_string(),
            grid_kwh: 6.0,
            self_kwh: 5.0,
            uncovered_kwh: 1.0,
            cost_cents: 150.0,
            baseline_cents: 330.0,
            feed_in_forgone_cents: 25.0,
        };
        save_tm_charge_cost(&pool, 1, &cost).await.unwrap();
        let todo = tm_charges_at_home(&pool, 1000).await.unwrap();
        assert_eq!(todo.iter().map(|t| t.id).collect::<Vec<_>>(), vec![5, 1]);
        assert_eq!(todo[1].cost_key.as_deref(), Some("k1"));
        assert_eq!(todo[0].cost_key, None);

        // Costing again replaces the figures; the session itself is untouched.
        let again = TmChargeCost {
            cost_key: "k2".to_string(),
            cost_cents: 99.0,
            ..cost
        };
        save_tm_charge_cost(&pool, 1, &again).await.unwrap();
        let row: (String, f64, f64, i64) = sqlx::query_as(
            "SELECT cost_key, cost_cents, energy_used_kwh, started_at FROM tm_charge WHERE id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, ("k2".to_string(), 99.0, 11.0, 2001));
        // A save keeps the cost when the session is read from TeslaMate again.
        save_tm_charges(&pool, &[tm_charge(1, 1, Some(100))])
            .await
            .unwrap();
        let key: Option<String> = sqlx::query_scalar("SELECT cost_key FROM tm_charge WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(key.as_deref(), Some("k2"));
    }

    #[tokio::test]
    async fn tm_history_is_read_per_car() {
        let (_dir, pool) = test_db().await;
        assert!(tm_cars(&pool).await.unwrap().is_empty());
        let car = |id: i64, name: &str| TmCar {
            id,
            name: Some(name.to_string()),
            ..TmCar::default()
        };
        upsert_tm_cars(&pool, &[car(10001, "B"), car(1, "A")], 5)
            .await
            .unwrap();
        let ids: Vec<i64> = tm_cars(&pool).await.unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 10001]);

        let day = |car_id: i64, date: &str, km: f64| TmDriveDay {
            car_id,
            date: date.to_string(),
            km,
            drives: 2,
        };
        upsert_tm_drive_days(
            &pool,
            &[
                day(1, "2026-10-21", 3.0),
                day(1, "2026-10-20", 12.5),
                day(10001, "2026-10-20", 99.0),
            ],
        )
        .await
        .unwrap();
        let days = tm_drive_days(&pool, 1).await.unwrap();
        assert_eq!(
            days,
            vec![day(1, "2026-10-20", 12.5), day(1, "2026-10-21", 3.0)]
        );
        assert!(tm_drive_days(&pool, 7).await.unwrap().is_empty());

        let mut away = tm_charge(2, 1, Some(100));
        away.geofence = None;
        save_tm_charges(
            &pool,
            &[tm_charge(3, 1, Some(100)), away, tm_charge(4, 10001, None)],
        )
        .await
        .unwrap();
        let cost = TmChargeCost {
            cost_key: "k".to_string(),
            grid_kwh: 6.0,
            self_kwh: 5.0,
            uncovered_kwh: 1.0,
            cost_cents: 150.0,
            baseline_cents: 330.0,
            feed_in_forgone_cents: 25.0,
        };
        save_tm_charge_cost(&pool, 3, &cost).await.unwrap();
        let rows = tm_charge_rows(&pool, 1).await.unwrap();
        // Oldest first; the costed one carries its figures, the other none.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].started_at, 2002);
        assert_eq!(rows[0].geofence, None);
        assert_eq!(rows[0].cost_key, None);
        assert_eq!(rows[0].cost_cents, None);
        assert_eq!(rows[1].energy_used_kwh, Some(11.0));
        assert_eq!(rows[1].cost_key.as_deref(), Some("k"));
        assert_eq!(rows[1].baseline_cents, Some(330.0));
        assert_eq!(rows[1].feed_in_forgone_cents, Some(25.0));
    }

    #[tokio::test]
    async fn deleting_a_manual_car_deletes_its_fills() {
        let (_dir, pool) = test_db().await;
        assert!(manual_cars(&pool).await.unwrap().is_empty());
        for name in ["A", "B"] {
            sqlx::query("INSERT INTO manual_car (name, created_at) VALUES (?, 5)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (car, date) in [(1, "2026-10-02"), (1, "2026-10-01"), (2, "2026-10-03")] {
            sqlx::query(
                "INSERT INTO fuel_fill (car_id, date, litres, cost_dollars) VALUES (?, ?, 40, 80)",
            )
            .bind(car)
            .bind(date)
            .execute(&pool)
            .await
            .unwrap();
        }
        let cars = manual_cars(&pool).await.unwrap();
        assert_eq!(
            (cars[0].name.as_str(), cars[0].kind.as_str()),
            ("A", "phev")
        );
        assert_eq!((cars.len(), &cars[0].model), (2, &None));

        // By car, then oldest first; a fill is a full tank unless said otherwise.
        let fills = fuel_fills(&pool).await.unwrap();
        let order: Vec<(i64, &str)> = fills.iter().map(|f| (f.car_id, f.date.as_str())).collect();
        assert_eq!(
            order,
            [(1, "2026-10-01"), (1, "2026-10-02"), (2, "2026-10-03")]
        );
        assert!(fills[0].full_tank && fills[0].note.is_empty());

        sqlx::query("DELETE FROM manual_car WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let left = fuel_fills(&pool).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].car_id, 2);
    }
}
