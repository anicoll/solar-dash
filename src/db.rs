//! SQLite storage: opening the database, migrations and small typed queries.
//!
//! We use sqlx's runtime API (`sqlx::query`, `query_as`) instead of the
//! compile-time `query!` macros. The macros check SQL against a real database
//! while compiling, which needs DATABASE_URL or offline data; the runtime API
//! keeps builds simple at the cost of checking SQL only when it runs (the
//! tests below cover that).

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::modbus::Sample;

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

/// Keeps the unparsed API response, so data can be re-processed later.
pub async fn save_raw_response(
    pool: &SqlitePool,
    endpoint: &str,
    window_start: Option<i64>,
    window_end: Option<i64>,
    body: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO raw_response (endpoint, window_start, window_end, fetched_at, body)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(endpoint)
    .bind(window_start)
    .bind(window_end)
    .bind(now())
    .bind(body)
    .execute(pool)
    .await
    .context("could not save raw response")?;
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
             ('point', 'reading', 'raw_response', 'sync_state', 'api_call')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tables.len(), 5);
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
    async fn raw_response_is_saved() {
        let (_dir, pool) = test_db().await;
        save_raw_response(&pool, "/x", Some(1), None, "{}")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM raw_response")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
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
}
