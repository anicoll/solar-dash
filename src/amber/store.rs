//! Saving Amber prices in the neutral `price_interval` table, and reading
//! them back for the API.
//!
//! The table is not Amber-specific (`source` says where a row came from), so
//! the names here are the neutral ones: channels are `general`, `feed_in` and
//! `controlled_load`, kinds are `actual`, `current` and `forecast`.

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;

use super::models::{ChannelType, IntervalKind, PriceInterval};

/// Forecast rows that ended more than this long ago are deleted. Normally an
/// actual price replaces them; this clears the ones that never were.
pub const FORECAST_KEEP_SECS: i64 = 2 * 24 * 3600;

/// The source name for an Amber site.
pub fn source_name(site_id: &str) -> String {
    format!("amber:{site_id}")
}

/// The table's channel name for an Amber channel. `None` for a channel we do
/// not know, which is skipped instead of stored under a made-up name.
fn channel_name(channel: ChannelType) -> Option<&'static str> {
    match channel {
        ChannelType::General => Some("general"),
        ChannelType::FeedIn => Some("feed_in"),
        ChannelType::ControlledLoad => Some("controlled_load"),
        ChannelType::Unknown => None,
    }
}

/// `None` for a kind we do not know.
fn kind_name(kind: IntervalKind) -> Option<&'static str> {
    match kind {
        IntervalKind::ActualInterval => Some("actual"),
        IntervalKind::CurrentInterval => Some("current"),
        IntervalKind::ForecastInterval => Some("forecast"),
        IntervalKind::Unknown => None,
    }
}

/// Saves the intervals, in one transaction. Returns how many were given a
/// row to write (rows with an unknown channel or kind are skipped).
///
/// An actual price is final, so it is never overwritten by a current or
/// forecast one (an old reply arriving late must not undo it). Current and
/// forecast overwrite each other, and an actual replaces either. The
/// `WHERE` on the `DO UPDATE` is what enforces that: when it is false the
/// existing row is left as it is.
pub async fn save_prices(
    pool: &SqlitePool,
    source: &str,
    intervals: &[PriceInterval],
    now: i64,
) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let mut saved = 0;
    for interval in intervals {
        let (Some(channel), Some(kind)) = (
            channel_name(interval.channel_type),
            kind_name(interval.kind),
        ) else {
            continue;
        };
        sqlx::query(
            "INSERT INTO price_interval
               (source, channel, start_ts, end_ts, cents, spot_cents, kind, fetched_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (source, channel, start_ts) DO UPDATE SET
               end_ts = excluded.end_ts, cents = excluded.cents,
               spot_cents = excluded.spot_cents, kind = excluded.kind,
               fetched_at = excluded.fetched_at
             WHERE price_interval.kind <> 'actual' OR excluded.kind = 'actual'",
        )
        .bind(source)
        .bind(channel)
        .bind(interval.start_time.timestamp())
        .bind(interval.end_time.timestamp())
        .bind(interval.per_kwh)
        .bind(interval.spot_per_kwh)
        .bind(kind)
        .bind(now)
        .execute(&mut *tx)
        .await
        .context("could not save a price interval")?;
        saved += 1;
    }
    tx.commit().await?;
    Ok(saved)
}

/// Deletes forecast rows that ended more than [`FORECAST_KEEP_SECS`] before
/// `now`. Actual prices are never deleted. Returns how many rows went.
pub async fn prune_forecasts(pool: &SqlitePool, now: i64) -> Result<u64> {
    let result = sqlx::query("DELETE FROM price_interval WHERE kind = 'forecast' AND end_ts < ?")
        .bind(now - FORECAST_KEEP_SECS)
        .execute(pool)
        .await
        .context("could not prune old forecast prices")?;
    Ok(result.rows_affected())
}

/// One stored interval, as the API sends it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct StoredPrice {
    #[serde(skip)]
    pub channel: String,
    pub start: i64,
    pub end: i64,
    /// c/kWh including GST, as the source gave it (feed-in sign not flipped).
    pub cents: f64,
    pub spot_cents: Option<f64>,
    pub kind: String,
}

/// Intervals that start from `from` up to (not including) `to`, oldest first.
/// With a `source` only that one's rows are read.
pub async fn prices_between(
    pool: &SqlitePool,
    source: Option<&str>,
    from: i64,
    to: i64,
) -> Result<Vec<StoredPrice>> {
    sqlx::query_as(
        "SELECT channel, start_ts AS start, end_ts AS end, cents, spot_cents, kind
         FROM price_interval
         WHERE start_ts >= ? AND start_ts < ? AND (? IS NULL OR source = ?)
         ORDER BY channel, start_ts",
    )
    .bind(from)
    .bind(to)
    .bind(source)
    .bind(source)
    .fetch_all(pool)
    .await
    .context("could not read prices")
}

/// The interval each channel is in at `now` (`start <= now < end`).
pub async fn prices_at(
    pool: &SqlitePool,
    source: Option<&str>,
    now: i64,
) -> Result<Vec<StoredPrice>> {
    sqlx::query_as(
        "SELECT channel, start_ts AS start, end_ts AS end, cents, spot_cents, kind
         FROM price_interval
         WHERE start_ts <= ? AND end_ts > ? AND (? IS NULL OR source = ?)
         ORDER BY channel, start_ts DESC",
    )
    .bind(now)
    .bind(now)
    .bind(source)
    .bind(source)
    .fetch_all(pool)
    .await
    .context("could not read the current prices")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn interval(kind: IntervalKind, channel: ChannelType, start: i64, cents: f64) -> PriceInterval {
        let at = |ts| -> DateTime<Utc> { DateTime::from_timestamp(ts, 0).unwrap() };
        PriceInterval {
            kind,
            channel_type: channel,
            per_kwh: cents,
            spot_per_kwh: cents / 2.0,
            duration: 5,
            start_time: at(start),
            end_time: at(start + 300),
            date: None,
            renewables: None,
            descriptor: None,
            spike_status: None,
            estimate: None,
            range: None,
            advanced_price: None,
        }
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    async fn kind_and_cents(pool: &SqlitePool, start: i64) -> (String, f64) {
        sqlx::query_as("SELECT kind, cents FROM price_interval WHERE start_ts = ?")
            .bind(start)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_actual_is_never_overwritten_by_current_or_forecast() {
        use ChannelType::General;
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let save = |kind, cents| {
            let pool = pool.clone();
            async move {
                save_prices(&pool, "amber:s", &[interval(kind, General, 1000, cents)], 5)
                    .await
                    .unwrap()
            }
        };

        // Forecast, then current, overwrite each other.
        save(ForecastInterval, 10.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("forecast".into(), 10.0));
        save(CurrentInterval, 11.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("current".into(), 11.0));
        save(ForecastInterval, 12.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("forecast".into(), 12.0));

        // An actual replaces them, and then nothing but an actual does.
        save(ActualInterval, 13.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("actual".into(), 13.0));
        save(CurrentInterval, 14.0).await;
        save(ForecastInterval, 15.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("actual".into(), 13.0));
        save(ActualInterval, 16.0).await;
        assert_eq!(kind_and_cents(&pool, 1000).await, ("actual".into(), 16.0));
    }

    #[tokio::test]
    async fn rows_are_neutral_and_unknowns_are_skipped() {
        let (_dir, pool) = pool().await;
        let rows = [
            interval(IntervalKind::ActualInterval, ChannelType::FeedIn, 0, -4.2),
            interval(
                IntervalKind::ActualInterval,
                ChannelType::ControlledLoad,
                0,
                9.0,
            ),
            interval(IntervalKind::ActualInterval, ChannelType::Unknown, 0, 1.0),
            interval(IntervalKind::Unknown, ChannelType::General, 0, 1.0),
        ];
        assert_eq!(save_prices(&pool, "amber:s", &rows, 5).await.unwrap(), 2);
        let all = prices_between(&pool, None, 0, 1).await.unwrap();
        let names: Vec<_> = all.iter().map(|p| p.channel.as_str()).collect();
        assert_eq!(names, ["controlled_load", "feed_in"]);
        // The sign is kept exactly as the source gave it.
        assert_eq!(all[1].cents, -4.2);
    }

    #[tokio::test]
    async fn old_forecasts_are_pruned_but_actuals_stay() {
        use ChannelType::General;
        let (_dir, pool) = pool().await;
        let now = 10 * 24 * 3600;
        let old = now - FORECAST_KEEP_SECS - 1000;
        let rows = [
            interval(IntervalKind::ForecastInterval, General, old, 1.0),
            interval(IntervalKind::ActualInterval, General, old + 300, 2.0),
            interval(IntervalKind::ForecastInterval, General, now - 3600, 3.0),
        ];
        save_prices(&pool, "amber:s", &rows, now).await.unwrap();
        assert_eq!(prune_forecasts(&pool, now).await.unwrap(), 1);
        let left = prices_between(&pool, None, 0, now + 1).await.unwrap();
        assert_eq!(left.len(), 2);
    }

    #[tokio::test]
    async fn the_current_interval_is_found_by_time() {
        use ChannelType::General;
        let (_dir, pool) = pool().await;
        let rows = [
            interval(IntervalKind::ActualInterval, General, 0, 1.0),
            interval(IntervalKind::CurrentInterval, General, 300, 2.0),
            interval(IntervalKind::ForecastInterval, General, 600, 3.0),
        ];
        save_prices(&pool, "amber:s", &rows, 0).await.unwrap();
        let now = prices_at(&pool, None, 450).await.unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].cents, 2.0);
        // The end is exclusive: at 600 the next interval is current.
        assert_eq!(prices_at(&pool, None, 600).await.unwrap()[0].cents, 3.0);
        assert!(prices_at(&pool, None, 9000).await.unwrap().is_empty());
    }
}
