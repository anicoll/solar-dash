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

    let pool = SqlitePoolOptions::new()
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
}
