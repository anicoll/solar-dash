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
use crate::cost::{DayPrices, PriceSpan};

/// Forecast rows that ended more than this long ago are deleted. Normally an
/// actual price replaces them; this clears the ones that never were.
pub const FORECAST_KEEP_SECS: i64 = 2 * 24 * 3600;

/// Amber gives each interval a `startTime` one second after the boundary
/// (`12:00:01`) but an `endTime` on it (`12:05:00`, or `12:30:00` for a
/// 30-minute row). Every reader wants the start on the boundary, so
/// [`save_prices`] floors it to the whole minute. Real interval starts are
/// always on a minute, so this only removes the extra second.
pub fn normalise_start(start_ts: i64) -> i64 {
    start_ts - start_ts.rem_euclid(60)
}

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
///
/// Start times are stored on the minute (see [`normalise_start`]), so the
/// primary key, the actual-wins rule and the overlap delete below all see the
/// same boundary whichever way Amber rounds it.
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
        let start_ts = normalise_start(interval.start_time.timestamp());
        sqlx::query(
            "INSERT INTO price_interval
               (source, channel, start_ts, end_ts, cents, spot_cents, kind, fetched_at,
                descriptor, renewables)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (source, channel, start_ts) DO UPDATE SET
               end_ts = excluded.end_ts, cents = excluded.cents,
               spot_cents = excluded.spot_cents, kind = excluded.kind,
               fetched_at = excluded.fetched_at, descriptor = excluded.descriptor,
               renewables = excluded.renewables
             WHERE price_interval.kind <> 'actual' OR excluded.kind = 'actual'",
        )
        .bind(source)
        .bind(channel)
        .bind(start_ts)
        .bind(interval.end_time.timestamp())
        .bind(interval.per_kwh)
        .bind(interval.spot_per_kwh)
        .bind(kind)
        .bind(now)
        .bind(interval.descriptor.as_deref())
        .bind(interval.renewables)
        .execute(&mut *tx)
        .await
        .context("could not save a price interval")?;
        // A longer forecast row (say 30 minutes) replacing 5-minute ones would
        // otherwise sit on top of the stale shorter rows it now covers, which
        // is a double count in the slot row. Drop forecast rows that lie
        // entirely inside the row just saved.
        if kind != "actual" {
            sqlx::query(
                "DELETE FROM price_interval
                 WHERE source = ? AND channel = ? AND kind = 'forecast'
                   AND start_ts > ? AND end_ts <= ?",
            )
            .bind(source)
            .bind(channel)
            .bind(start_ts)
            .bind(interval.end_time.timestamp())
            .execute(&mut *tx)
            .await
            .context("could not drop forecasts covered by a longer interval")?;
        }
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

/// The start time of the oldest `actual` price stored for a source, or `None`
/// when there is none. The backfill walks back from here.
pub async fn oldest_actual_start(pool: &SqlitePool, source: &str) -> Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT MIN(start_ts) FROM price_interval WHERE source = ? AND kind = 'actual'",
    )
    .bind(source)
    .fetch_one(pool)
    .await
    .context("could not find the oldest stored price")
}

/// One stored interval, as the API sends it.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct StoredPrice {
    #[serde(skip)]
    pub channel: String,
    pub start: i64,
    pub end: i64,
    /// c/kWh including GST, as the source gave it. Not flipped: a negative
    /// feed-in price means the customer is paid (Amber's convention).
    pub cents: f64,
    pub spot_cents: Option<f64>,
    pub kind: String,
    /// Unix seconds when we stored this row. For a `current` row this is when
    /// its price was last refreshed (the Today header shows it as "settled").
    pub fetched_at: i64,
    /// Amber's label: extremelyLow, veryLow, low, neutral, high or spike.
    pub descriptor: Option<String>,
    /// Percent of the grid that is renewable, when known.
    pub renewables: Option<f64>,
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
        "SELECT channel, start_ts AS start, end_ts AS end, cents, spot_cents, kind,
                fetched_at, descriptor, renewables
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

/// How far back from `now` [`prices_at`] looks for a row that started before
/// it. Rows are 5 or 30 minutes long; a day is far more than any stored
/// interval, and it lets SQLite use the `start_ts` index as a short range
/// instead of walking it from the oldest row.
const CURRENT_LOOKBACK_SECS: i64 = 24 * 3600;

/// The interval each channel is in at `now` (`start <= now < end`).
pub async fn prices_at(
    pool: &SqlitePool,
    source: Option<&str>,
    now: i64,
) -> Result<Vec<StoredPrice>> {
    sqlx::query_as(
        "SELECT channel, start_ts AS start, end_ts AS end, cents, spot_cents, kind,
                fetched_at, descriptor, renewables
         FROM price_interval
         WHERE start_ts <= ? AND start_ts > ? AND end_ts > ? AND (? IS NULL OR source = ?)
         ORDER BY channel, start_ts DESC",
    )
    .bind(now)
    .bind(now - CURRENT_LOOKBACK_SECS)
    .bind(now)
    .bind(source)
    .bind(source)
    .fetch_all(pool)
    .await
    .context("could not read the current prices")
}

// Both queries below keep the same rows ("known" prices): finished `actual`
// ones, plus `current` (the interval under way) when `include_current`.
// Forecasts are never used: they are guesses, not what was charged.

/// The import and feed-in prices of one local day `[start, end)` for costing,
/// from every source whose name starts with `source_prefix`.
pub async fn day_prices(
    pool: &SqlitePool,
    source_prefix: &str,
    start: i64,
    end: i64,
    include_current: bool,
) -> Result<DayPrices> {
    let rows: Vec<(String, i64, i64, f64)> = sqlx::query_as(
        "SELECT channel, start_ts, end_ts, cents FROM price_interval
         WHERE substr(source, 1, length(?)) = ? AND channel IN ('general', 'feed_in')
           AND start_ts >= ? AND start_ts < ? AND (kind = 'actual' OR (? AND kind = 'current'))
         ORDER BY start_ts, source",
    )
    .bind(source_prefix)
    .bind(source_prefix)
    .bind(start)
    .bind(end)
    .bind(include_current)
    .fetch_all(pool)
    .await
    .context("could not read the day's prices")?;
    let (mut general, mut feed_in) = (Vec::new(), Vec::new());
    for (channel, start_ts, end_ts, cents) in rows {
        let span = PriceSpan {
            start_ts,
            end_ts,
            cents,
        };
        if channel == "general" {
            general.push(span);
        } else {
            feed_in.push(span);
        }
    }
    Ok(DayPrices::new(general, feed_in))
}

/// The `general` (buy) prices that start in `[start, end)`, oldest first, for
/// the daily stats. Finished `actual` rows only, plus `current` rows that
/// start at or after `current_from` (the start of today, so the interval
/// under way counts today and a stale `current` row never counts on an older
/// day). Forecasts are never read. Rows are skipped when another site's source
/// already gave the same start time, so a second site does not double count.
pub async fn known_general_prices(
    pool: &SqlitePool,
    source_prefix: &str,
    start: i64,
    end: i64,
    current_from: i64,
) -> Result<Vec<PriceSpan>> {
    let rows: Vec<(i64, i64, f64)> = sqlx::query_as(
        "SELECT start_ts, end_ts, cents FROM price_interval
         WHERE substr(source, 1, length(?)) = ? AND channel = 'general'
           AND start_ts >= ? AND start_ts < ?
           AND (kind = 'actual' OR (kind = 'current' AND start_ts >= ?))
         ORDER BY start_ts, source",
    )
    .bind(source_prefix)
    .bind(source_prefix)
    .bind(start)
    .bind(end)
    .bind(current_from)
    .fetch_all(pool)
    .await
    .context("could not read the known general prices")?;
    let mut spans: Vec<PriceSpan> = Vec::with_capacity(rows.len());
    for (start_ts, end_ts, cents) in rows {
        if spans.last().is_some_and(|last| last.start_ts == start_ts) {
            continue;
        }
        spans.push(PriceSpan {
            start_ts,
            end_ts,
            cents,
        });
    }
    Ok(spans)
}

/// A short text that changes when the rows [`day_prices`] would return
/// change: how many there are, the sum of their prices, the sum of their
/// start times (a price moved in time changes it) and the newest
/// `fetched_at`. A stored day cost is tagged with it, so a late actual price
/// (or a corrected one) makes the day be costed again.
pub async fn day_prices_fingerprint(
    pool: &SqlitePool,
    source_prefix: &str,
    start: i64,
    end: i64,
    include_current: bool,
) -> Result<String> {
    let (count, sum, starts, newest): (i64, f64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(cents), 0.0), COALESCE(SUM(start_ts), 0),
                COALESCE(MAX(fetched_at), 0)
         FROM price_interval
         WHERE substr(source, 1, length(?)) = ? AND channel IN ('general', 'feed_in')
           AND start_ts >= ? AND start_ts < ? AND (kind = 'actual' OR (? AND kind = 'current'))",
    )
    .bind(source_prefix)
    .bind(source_prefix)
    .bind(start)
    .bind(end)
    .bind(include_current)
    .fetch_one(pool)
    .await
    .context("could not fingerprint the day's prices")?;
    Ok(format!("{count}:{sum:.4}:{starts}:{newest}"))
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
                save_prices(&pool, "amber:s", &[interval(kind, General, 1020, cents)], 5)
                    .await
                    .unwrap()
            }
        };

        // Forecast, then current, overwrite each other.
        save(ForecastInterval, 10.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("forecast".into(), 10.0));
        save(CurrentInterval, 11.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("current".into(), 11.0));
        save(ForecastInterval, 12.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("forecast".into(), 12.0));

        // An actual replaces them, and then nothing but an actual does.
        save(ActualInterval, 13.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("actual".into(), 13.0));
        save(CurrentInterval, 14.0).await;
        save(ForecastInterval, 15.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("actual".into(), 13.0));
        save(ActualInterval, 16.0).await;
        assert_eq!(kind_and_cents(&pool, 1020).await, ("actual".into(), 16.0));
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

    #[tokio::test]
    async fn descriptor_and_renewables_round_trip() {
        use ChannelType::General;
        let (_dir, pool) = pool().await;
        let mut with = interval(IntervalKind::CurrentInterval, General, 300, 32.0);
        with.descriptor = Some("low".into());
        with.renewables = Some(35.0);
        let without = interval(IntervalKind::ForecastInterval, General, 600, 30.0);
        save_prices(&pool, "amber:s", &[with, without], 0)
            .await
            .unwrap();
        let rows = prices_between(&pool, None, 0, 1000).await.unwrap();
        assert_eq!(rows[0].descriptor.as_deref(), Some("low"));
        assert_eq!(rows[0].renewables, Some(35.0));
        assert_eq!(rows[1].descriptor, None);
        assert_eq!(rows[1].renewables, None);
        // A later save with new labels replaces them, and clears missing ones.
        let mut again = interval(IntervalKind::CurrentInterval, General, 300, 33.0);
        again.descriptor = Some("neutral".into());
        save_prices(&pool, "amber:s", &[again], 1).await.unwrap();
        let rows = prices_between(&pool, None, 0, 1000).await.unwrap();
        assert_eq!(rows[0].descriptor.as_deref(), Some("neutral"));
        assert_eq!(rows[0].renewables, None);
    }

    #[tokio::test]
    async fn a_longer_forecast_replaces_the_short_rows_it_covers() {
        use ChannelType::General;
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        // Stale 5-minute forecasts for 1800..2400, then a 30-minute one over them.
        let short: Vec<_> = (0..6)
            .map(|i| interval(ForecastInterval, General, 1800 + i * 300, 10.0))
            .collect();
        save_prices(&pool, "amber:s", &short, 0).await.unwrap();
        let mut long = interval(ForecastInterval, General, 1800, 20.0);
        long.end_time = DateTime::from_timestamp(3600, 0).unwrap();
        save_prices(&pool, "amber:s", &[long], 1).await.unwrap();
        let rows = prices_between(&pool, None, 0, 9000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].start, rows[0].end, rows[0].cents),
            (1800, 3600, 20.0)
        );
        // The other way round (5-minute over 30-minute) leaves a gap, not a double.
        save_prices(
            &pool,
            "amber:s",
            &[interval(ForecastInterval, General, 1800, 5.0)],
            2,
        )
        .await
        .unwrap();
        let rows = prices_between(&pool, None, 0, 9000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].end, 2100);
    }

    #[tokio::test]
    async fn now_prefers_the_newest_start_among_overlapping_rows() {
        use ChannelType::General;
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let mut long = interval(ForecastInterval, General, 0, 50.0);
        long.end_time = DateTime::from_timestamp(1800, 0).unwrap();
        let short = interval(CurrentInterval, General, 900, 32.0);
        save_prices(&pool, "amber:s", &[long, short], 0)
            .await
            .unwrap();
        let now = prices_at(&pool, None, 1000).await.unwrap();
        assert_eq!(now.len(), 2);
        assert_eq!(now[0].cents, 32.0); // newest start first
    }

    #[tokio::test]
    async fn day_prices_skip_forecasts_and_current_on_finished_days() {
        use ChannelType::{FeedIn, General};
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let rows = [
            interval(ActualInterval, General, 0, 10.0),
            interval(CurrentInterval, General, 300, 20.0),
            interval(ForecastInterval, General, 600, 30.0),
            interval(ActualInterval, FeedIn, 0, -4.0),
        ];
        save_prices(&pool, "amber:s", &rows, 5).await.unwrap();
        // Another provider's rows are not matched by the prefix.
        let other = [interval(ActualInterval, General, 900, 99.0)];
        save_prices(&pool, "other:s", &other, 5).await.unwrap();

        let finished = day_prices(&pool, "amber", 0, 86_400, false).await.unwrap();
        assert_eq!(finished.general.len(), 1);
        assert_eq!(finished.feed_in.len(), 1);
        // Today: the interval under way counts too, still never the forecast.
        let today = day_prices(&pool, "amber", 0, 86_400, true).await.unwrap();
        let cents: Vec<f64> = today.general.iter().map(|s| s.cents).collect();
        assert_eq!(cents, [10.0, 20.0]);
    }

    #[tokio::test]
    async fn the_fingerprint_changes_when_a_price_row_changes() {
        use ChannelType::General;
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let fp = || day_prices_fingerprint(&pool, "amber", 0, 86_400, false);
        let empty = fp().await.unwrap();
        save_prices(
            &pool,
            "amber:s",
            &[interval(ActualInterval, General, 0, 10.0)],
            5,
        )
        .await
        .unwrap();
        let one = fp().await.unwrap();
        assert_ne!(empty, one);
        // A forecast is not a known price, so it does not move the fingerprint.
        save_prices(
            &pool,
            "amber:s",
            &[interval(ForecastInterval, General, 600, 7.0)],
            6,
        )
        .await
        .unwrap();
        assert_eq!(fp().await.unwrap(), one);
        // A late actual adds a row.
        save_prices(
            &pool,
            "amber:s",
            &[interval(ActualInterval, General, 300, 12.0)],
            7,
        )
        .await
        .unwrap();
        let two = fp().await.unwrap();
        assert_ne!(two, one);
        // A corrected price at the same time changes the sum and fetched_at.
        save_prices(
            &pool,
            "amber:s",
            &[interval(ActualInterval, General, 300, 13.0)],
            8,
        )
        .await
        .unwrap();
        assert_ne!(fp().await.unwrap(), two);
    }

    /// An Amber-style interval: the start one second after the boundary, the
    /// end on it, `minutes` long.
    fn amber_style(kind: IntervalKind, boundary: i64, minutes: i64, cents: f64) -> PriceInterval {
        let at = |ts| -> DateTime<Utc> { DateTime::from_timestamp(ts, 0).unwrap() };
        let mut row = interval(kind, ChannelType::General, boundary + 1, cents);
        row.duration = minutes as u32;
        row.end_time = at(boundary + minutes * 60);
        row
    }

    // 2026-11-11 02:00:00 UTC, a multiple of 1800.
    const T0: i64 = 1_794_362_400;

    #[tokio::test]
    async fn starts_are_stored_on_the_boundary() {
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let rows = [
            amber_style(ActualInterval, T0, 5, 10.0),
            amber_style(CurrentInterval, T0 + 300, 5, 11.0),
            amber_style(ForecastInterval, T0 + 1800, 30, 12.0),
        ];
        save_prices(&pool, "amber:s", &rows, 5).await.unwrap();
        let stored = prices_between(&pool, None, T0, T0 + 7200).await.unwrap();
        let spans: Vec<_> = stored.iter().map(|p| (p.start, p.end)).collect();
        assert_eq!(
            spans,
            [(T0, T0 + 300), (T0 + 300, T0 + 600), (T0 + 1800, T0 + 3600)]
        );
        assert_eq!(normalise_start(-59), -60);
    }

    #[tokio::test]
    async fn a_shifted_start_still_obeys_the_actual_and_overlap_rules() {
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        // Actual at :00:01 is not overwritten by a later forecast at :00:01.
        save_prices(
            &pool,
            "amber:s",
            &[amber_style(ActualInterval, T0, 5, 13.0)],
            1,
        )
        .await
        .unwrap();
        save_prices(
            &pool,
            "amber:s",
            &[amber_style(ForecastInterval, T0, 5, 99.0)],
            2,
        )
        .await
        .unwrap();
        assert_eq!(kind_and_cents(&pool, T0).await, ("actual".into(), 13.0));
        // A 30-minute forecast drops the 5-minute forecasts inside it.
        let short: Vec<_> = (1..6)
            .map(|i| amber_style(ForecastInterval, T0 + i * 300, 5, 1.0))
            .collect();
        save_prices(&pool, "amber:s", &short, 3).await.unwrap();
        save_prices(
            &pool,
            "amber:s",
            &[amber_style(ForecastInterval, T0, 30, 7.0)],
            4,
        )
        .await
        .unwrap();
        let rows = prices_between(&pool, None, T0, T0 + 1800).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].kind.as_str(), rows[0].cents), ("actual", 13.0));
    }

    #[tokio::test]
    async fn a_current_row_with_a_one_second_start_wins_prices_at() {
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let rows = [
            amber_style(ForecastInterval, T0, 30, 50.0),
            amber_style(CurrentInterval, T0 + 600, 5, 32.0),
        ];
        save_prices(&pool, "amber:s", &rows, 9).await.unwrap();
        // Exactly on the boundary, which the +1 s start would have missed.
        let now = prices_at(&pool, None, T0 + 600).await.unwrap();
        assert_eq!(now.len(), 2);
        assert_eq!((now[0].start, now[0].cents), (T0 + 600, 32.0));
        assert_eq!(now[0].fetched_at, 9);
    }

    #[tokio::test]
    async fn the_migration_moves_old_rows_to_the_boundary() {
        let (_dir, pool) = pool().await;
        let insert = |start: i64, kind: &'static str, cents: f64| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO price_interval
                       (source, channel, start_ts, end_ts, cents, kind, fetched_at)
                     VALUES ('amber:s', 'general', ?, ?, ?, ?, 1)",
                )
                .bind(start)
                .bind(start + 300)
                .bind(cents)
                .bind(kind)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        insert(T0 + 1, "actual", 1.0).await; // just moves
        insert(T0 + 301, "forecast", 2.0).await; // shifted, collides with...
        insert(T0 + 300, "actual", 3.0).await; // ...an actual: the actual stays
        insert(T0 + 601, "actual", 4.0).await; // shifted actual, collides with...
        insert(T0 + 600, "forecast", 5.0).await; // ...a forecast: the actual wins
        insert(T0 + 900, "current", 6.0).await; // already on the boundary
        sqlx::raw_sql(include_str!(
            "../../migrations/0022_price_start_on_boundary.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let rows: Vec<(i64, String, f64)> =
            sqlx::query_as("SELECT start_ts, kind, cents FROM price_interval ORDER BY start_ts")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            [
                (T0, "actual".to_string(), 1.0),
                (T0 + 300, "actual".to_string(), 3.0),
                (T0 + 600, "actual".to_string(), 4.0),
                (T0 + 900, "current".to_string(), 6.0),
            ]
        );
    }

    #[tokio::test]
    async fn now_ignores_rows_older_than_the_lookback() {
        use ChannelType::General;
        use IntervalKind::*;
        let (_dir, pool) = pool().await;
        let now = 10 * CURRENT_LOOKBACK_SECS;
        // A (bogus) row that started two days ago and is still open is out of
        // reach; one that started a minute ago is found.
        let mut stale = interval(
            ActualInterval,
            General,
            now - 2 * CURRENT_LOOKBACK_SECS,
            1.0,
        );
        stale.end_time = DateTime::from_timestamp(now + 300, 0).unwrap();
        let fresh = interval(CurrentInterval, General, now - 60, 2.0);
        save_prices(&pool, "amber:s", &[stale, fresh], 0)
            .await
            .unwrap();
        let rows = prices_at(&pool, None, now).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cents, 2.0);
    }
}
