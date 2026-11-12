//! The database side of the Home Assistant source: saving readings and
//! reading them back.

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use super::{Reading, SOURCE_NAME};

/// Saves readings in one transaction: each into `ha_reading`, and the ones
/// mapped to a metric also into `metric_reading` under the source name
/// `home-assistant`. Saving the same reading again just overwrites it.
pub async fn save(pool: &SqlitePool, readings: &[Reading]) -> Result<()> {
    if readings.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for reading in readings {
        sqlx::query(
            "INSERT INTO ha_reading (entity_id, ts, value, unit) VALUES (?, ?, ?, ?)
             ON CONFLICT (entity_id, ts) DO UPDATE SET value = excluded.value, unit = excluded.unit",
        )
        .bind(&reading.entity_id)
        .bind(reading.ts)
        .bind(reading.value)
        .bind(&reading.unit)
        .execute(&mut *tx)
        .await
        .context("could not save Home Assistant reading")?;
        if let Some((metric, value)) = reading.metric {
            sqlx::query(
                "INSERT INTO metric_reading (source, metric, ts, value) VALUES (?, ?, ?, ?)
                 ON CONFLICT (source, metric, ts) DO UPDATE SET value = excluded.value",
            )
            .bind(SOURCE_NAME)
            .bind(metric.name())
            .bind(reading.ts)
            .bind(value)
            .execute(&mut *tx)
            .await
            .context("could not save Home Assistant metric reading")?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// The newest stored `(ts, value, unit)` of an entity, if any.
pub async fn latest(
    pool: &SqlitePool,
    entity_id: &str,
) -> Result<Option<(i64, f64, Option<String>)>> {
    sqlx::query_as(
        "SELECT ts, value, unit FROM ha_reading WHERE entity_id = ? ORDER BY ts DESC LIMIT 1",
    )
    .bind(entity_id)
    .fetch_optional(pool)
    .await
    .context("could not read Home Assistant readings")
}

/// An entity's `(ts, value)` pairs with `from <= ts < to`, oldest first.
/// With `step` seconds the rows are averaged per `step`-second bucket (the
/// bucket's start is the time), so a long range stays small.
pub async fn history(
    pool: &SqlitePool,
    entity_id: &str,
    from: i64,
    to: i64,
    step: Option<i64>,
) -> Result<Vec<(i64, f64)>> {
    let rows = match step {
        Some(step) => {
            sqlx::query_as(
                "SELECT (ts / ?1) * ?1, AVG(value) FROM ha_reading
                 WHERE entity_id = ?2 AND ts >= ?3 AND ts < ?4
                 GROUP BY ts / ?1 ORDER BY 1",
            )
            .bind(step)
            .bind(entity_id)
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await
        }
        None => {
            sqlx::query_as(
                "SELECT ts, value FROM ha_reading
                 WHERE entity_id = ? AND ts >= ? AND ts < ? ORDER BY ts",
            )
            .bind(entity_id)
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await
        }
    };
    rows.context("could not read Home Assistant history")
}

/// The oldest stored reading time of an entity, if any.
pub async fn oldest_ts(pool: &SqlitePool, entity_id: &str) -> Result<Option<i64>> {
    sqlx::query_scalar("SELECT MIN(ts) FROM ha_reading WHERE entity_id = ?")
        .bind(entity_id)
        .fetch_one(pool)
        .await
        .context("could not read Home Assistant readings")
}
