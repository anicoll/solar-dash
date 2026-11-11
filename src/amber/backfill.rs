//! Backfill of past Amber prices, run by the price task between live fetches.
//!
//! Each pass walks backwards from the oldest `actual` price we hold (or from
//! yesterday when we hold none) in chunks of [`CHUNK_DAYS`] days, down to a
//! floor date (`backfill_from` in `[amber]`, default two years back). It stops
//! going further back when Amber returns nothing older, or says the range is
//! not allowed (a closed site may not serve old prices). That is remembered in
//! `sync_state`, so the next day it is not asked again.
//!
//! The position is never stored: the oldest stored actual price *is* the
//! cursor, so a restart picks up exactly where it stopped. Only the "Amber has
//! nothing more" answer needs a stored row.
//!
//! Everything here is read-only towards Amber (GET only).

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Days, NaiveDate, Utc};
use sqlx::SqlitePool;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::client::{Client, NotFound, Refused};
use super::models::{IntervalKind, PriceInterval};
use super::store;
use super::task::backoff_delay;
use crate::config::Amber;

/// Days asked for per request, both ends included. Amber limits the range of
/// a request; a week is the limit its usage endpoint documents.
pub const CHUNK_DAYS: u64 = 7;
/// Interval length to ask for, in minutes. The live fetch stores 5-minute rows.
const RESOLUTION: u32 = 5;
/// Pause between two requests, to be gentle with Amber.
const PAUSE: Duration = Duration::from_secs(2);
/// Chunks per 5-minute cycle at most, so the live fetch is never crowded out.
const CHUNKS_PER_CYCLE: usize = 3;
/// A chunk is only started with at least this long left before the next live
/// fetch (a request can take up to the client's 30 s timeout).
const MIN_TIME_LEFT: Duration = Duration::from_secs(45);
/// How often to look again once the backfill has reached its end.
const RECHECK: Duration = Duration::from_secs(24 * 3600);
/// Default for `backfill_from`: this many days back (two years).
const DEFAULT_DAYS_BACK: u64 = 2 * 365;

/// The oldest day to fetch: the configured date, or two years before `today`.
pub fn floor_date(configured: Option<NaiveDate>, today: NaiveDate) -> NaiveDate {
    configured.unwrap_or_else(|| today - Days::new(DEFAULT_DAYS_BACK))
}

/// The next date range to ask for, as `(start, end)` inclusive, or `None`
/// when there is nothing left to do.
///
/// `oldest` is the date of the oldest stored actual price. The new range ends
/// on that same day, not the day before: the stored day may be only partly
/// filled, and the date is a UTC date while Amber's are site-local, so
/// overlapping by a day is the way to be sure of leaving no gap. Rows already
/// stored are harmless to fetch again (actual prices are never overwritten).
/// With nothing stored the range ends yesterday.
pub fn plan_chunk(
    oldest: Option<NaiveDate>,
    today: NaiveDate,
    floor: NaiveDate,
) -> Option<(NaiveDate, NaiveDate)> {
    let end = match oldest {
        Some(day) if day <= floor => return None,
        Some(day) => day,
        None => today - Days::new(1),
    };
    if end < floor {
        return None;
    }
    let start = (end - Days::new(CHUNK_DAYS - 1)).max(floor);
    Some((start, end))
}

/// True when a reply shows there is nothing older to get: none of its rows
/// starts before `oldest_ts` (the oldest stored actual, if any). An empty
/// reply is the plain case of that.
pub fn nothing_older(items: &[PriceInterval], oldest_ts: Option<i64>) -> bool {
    match oldest_ts {
        Some(oldest) => !items.iter().any(|p| p.start_time.timestamp() < oldest),
        None => items.is_empty(),
    }
}

/// An error that means "Amber will not give this range", as opposed to a
/// passing failure worth retrying: the site is unknown or closed (404) or the
/// request was refused (400, 403, 422).
fn range_not_available(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotFound>().is_some() || err.downcast_ref::<Refused>().is_some()
}

/// Only final prices are kept from a backfill. A past range should hold
/// nothing else, but a current or forecast row must never get in beside an
/// actual one.
fn actuals_only(items: Vec<PriceInterval>) -> Vec<PriceInterval> {
    items
        .into_iter()
        .filter(|p| p.kind == IntervalKind::ActualInterval)
        .collect()
}

/// The `sync_state` job name that marks the backfill as finished for a site.
/// Its `cursor_ts` is the start of the oldest day asked for.
fn done_job(site_id: &str) -> String {
    format!("amber-backfill-done:{site_id}")
}

fn day_start_ts(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp()
}

/// The backfill's state between cycles. Lives in the price task.
pub struct Backfill {
    /// Do nothing before this moment (a day after finishing, or a backoff).
    not_before: Option<Instant>,
    failures: u32,
    /// Pause between requests; a field so tests can make it zero.
    pause: Duration,
}

impl Backfill {
    pub fn new() -> Self {
        Self {
            not_before: None,
            failures: 0,
            pause: PAUSE,
        }
    }

    /// Does a few chunks, and returns when they are done, when `cycle_ends`
    /// (the next live fetch) is too close for another, or on a failure.
    /// Never returns an error: trouble is logged and retried later with the
    /// usual backoff.
    pub async fn run_cycle(
        &mut self,
        client: &Client,
        pool: &SqlitePool,
        config: &Amber,
        site_id: &str,
        cycle_ends: Instant,
    ) {
        if self.not_before.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.not_before = None;
        for chunk in 0..CHUNKS_PER_CYCLE {
            if cycle_ends.saturating_duration_since(Instant::now()) < MIN_TIME_LEFT {
                return;
            }
            if chunk > 0 {
                tokio::time::sleep(self.pause).await;
            }
            match self.one_chunk(client, pool, config, site_id).await {
                Ok(true) => self.failures = 0,
                Ok(false) => {
                    // Finished: look again in a day.
                    self.not_before = Some(Instant::now() + RECHECK);
                    return;
                }
                Err(err) => {
                    self.failures = self.failures.saturating_add(1);
                    let wait = backoff_delay(self.failures);
                    warn!(
                        "Amber price backfill failed: {err:#}; trying again in {}s",
                        wait.as_secs()
                    );
                    self.not_before = Some(Instant::now() + wait);
                    return;
                }
            }
        }
    }

    /// Fetches and saves one chunk. `Ok(true)` means more may remain,
    /// `Ok(false)` that the backfill is finished (reached the floor, or
    /// Amber has nothing older).
    async fn one_chunk(
        &self,
        client: &Client,
        pool: &SqlitePool,
        config: &Amber,
        site_id: &str,
    ) -> Result<bool> {
        let job = done_job(site_id);
        if crate::db::get_cursor(pool, &job).await?.is_some() {
            return Ok(false);
        }
        let source = store::source_name(site_id);
        let oldest_ts = store::oldest_actual_start(pool, &source).await?;
        let oldest_day = oldest_ts
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .map(|t| t.date_naive());
        let today = Utc::now().date_naive();
        let floor = floor_date(config.backfill_from, today);
        let Some((start, end)) = plan_chunk(oldest_day, today, floor) else {
            debug!(%floor, "Amber price backfill has reached its start date");
            return Ok(false);
        };

        let items = match client.prices_between(site_id, start, end, RESOLUTION).await {
            Ok(reply) => reply.items,
            Err(err) if range_not_available(&err) => {
                finish(
                    pool,
                    &job,
                    start,
                    &format!("Amber would not give it: {err:#}"),
                )
                .await?;
                return Ok(false);
            }
            Err(err) => return Err(err),
        };
        let stop = nothing_older(&items, oldest_ts);
        let saved =
            store::save_prices(pool, &source, &actuals_only(items), Utc::now().timestamp()).await?;
        debug!(%start, %end, saved, "Amber past prices stored");
        if stop {
            finish(pool, &job, start, "Amber has no older prices").await?;
            return Ok(false);
        }
        Ok(true)
    }
}

/// Remembers that the backfill is finished and logs how far it got (once:
/// the stored row stops later passes from asking again).
async fn finish(pool: &SqlitePool, job: &str, reached: NaiveDate, why: &str) -> Result<()> {
    crate::db::set_cursor(pool, job, day_start_ts(reached)).await?;
    info!("Amber price backfill stopped at {reached}: {why}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn the_floor_defaults_to_two_years_back() {
        let today = day(2026, 11, 11);
        assert_eq!(floor_date(None, today), day(2024, 11, 11));
        assert_eq!(floor_date(Some(day(2025, 1, 1)), today), day(2025, 1, 1));
    }

    #[test]
    fn chunks_walk_back_and_stop_at_the_floor() {
        let today = day(2026, 11, 11);
        let floor = day(2026, 10, 20);
        // Nothing stored: the first chunk is the week ending yesterday.
        let first = plan_chunk(None, today, floor).unwrap();
        assert_eq!(first, (day(2026, 11, 4), day(2026, 11, 10)));
        // Then each chunk ends on the oldest stored day (a one-day overlap).
        let second = plan_chunk(Some(first.0), today, floor).unwrap();
        assert_eq!(second, (day(2026, 10, 29), day(2026, 11, 4)));
        // The last chunk is cut at the floor.
        let last = plan_chunk(Some(day(2026, 10, 24)), today, floor).unwrap();
        assert_eq!(last, (day(2026, 10, 20), day(2026, 10, 24)));
        // Reached the floor: nothing more.
        assert_eq!(plan_chunk(Some(floor), today, floor), None);
        assert_eq!(plan_chunk(Some(day(2026, 10, 1)), today, floor), None);
        // A floor in the future leaves nothing to fetch.
        assert_eq!(plan_chunk(None, today, day(2026, 12, 1)), None);
        // No chunk is longer than CHUNK_DAYS.
        let (s, e) = plan_chunk(None, today, day(2000, 1, 1)).unwrap();
        assert_eq!((e - s).num_days() as u64 + 1, CHUNK_DAYS);
    }

    fn actual_at(kind: IntervalKind, start: i64, cents: f64) -> PriceInterval {
        serde_json::from_value(serde_json::json!({
            "type": match kind {
                IntervalKind::ActualInterval => "ActualInterval",
                IntervalKind::CurrentInterval => "CurrentInterval",
                _ => "ForecastInterval",
            },
            "channelType": "general",
            "perKwh": cents,
            "spotPerKwh": cents,
            "duration": 5,
            "startTime": DateTime::from_timestamp(start, 0).unwrap(),
            "endTime": DateTime::from_timestamp(start + 300, 0).unwrap(),
        }))
        .unwrap()
    }

    #[test]
    fn stop_when_amber_has_nothing_older() {
        let row = |ts| actual_at(IntervalKind::ActualInterval, ts, 1.0);
        // Empty reply: stop, whether or not we hold anything.
        assert!(nothing_older(&[], None));
        assert!(nothing_older(&[], Some(1000)));
        // First ever chunk with rows: go on.
        assert!(!nothing_older(&[row(500)], None));
        // Only the overlap day (nothing before what we hold): stop.
        assert!(nothing_older(&[row(1000), row(1300)], Some(1000)));
        // Something older than what we hold: go on.
        assert!(!nothing_older(&[row(700), row(1000)], Some(1000)));
    }

    #[test]
    fn refused_ranges_and_closed_sites_end_the_backfill_other_errors_do_not() {
        assert!(range_not_available(&anyhow::Error::new(NotFound(
            "x".into()
        ))));
        assert!(range_not_available(&anyhow::Error::new(Refused(
            "x".into()
        ))));
        assert!(!range_not_available(&anyhow::anyhow!("connection reset")));
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn a_backfill_never_overwrites_an_actual() {
        let (_dir, pool) = pool().await;
        let start = 1_700_000_000;
        let now = 1_800_000_000;
        let first = actual_at(IntervalKind::ActualInterval, start, 10.0);
        store::save_prices(&pool, "amber:s1", &[first], now)
            .await
            .unwrap();
        // The chunk holds the same interval as a forecast, and as an actual
        // with a different price, plus a current row: only actuals pass the
        // filter, and the save itself never lets a non-actual win either.
        let chunk = vec![
            actual_at(IntervalKind::ForecastInterval, start, 99.0),
            actual_at(IntervalKind::CurrentInterval, start + 300, 98.0),
        ];
        assert!(actuals_only(chunk.clone()).is_empty());
        store::save_prices(&pool, "amber:s1", &chunk, now)
            .await
            .unwrap();
        let rows = store::prices_between(&pool, Some("amber:s1"), 0, i64::MAX)
            .await
            .unwrap();
        let at_start = rows.iter().find(|r| r.start == start).unwrap();
        assert_eq!((at_start.kind.as_str(), at_start.cents), ("actual", 10.0));
        assert_eq!(
            store::oldest_actual_start(&pool, "amber:s1").await.unwrap(),
            Some(start)
        );
    }

    /// Amber stand-in: `s1` returns the 2021 fixture for any range; `empty`
    /// returns nothing; `closed` refuses the range.
    async fn fake_amber() -> Amber {
        let app = Router::new()
            .route(
                "/v1/sites/s1/prices",
                get(|| async { include_str!("testdata/prices_current.json") }),
            )
            .route("/v1/sites/empty/prices", get(|| async { "[]" }))
            .route(
                "/v1/sites/closed/prices",
                get(|| async { (StatusCode::BAD_REQUEST, "range not allowed") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Amber {
            token: "t".to_string(),
            site_id: None,
            backfill_from: Some(day(2021, 1, 1)),
            base_url: format!("http://{addr}/v1"),
            compare: Default::default(),
        }
    }

    async fn cycle(backfill: &mut Backfill, config: &Amber, pool: &SqlitePool, site: &str) {
        let client = Client::new(config).unwrap();
        let ends = Instant::now() + Duration::from_secs(300);
        backfill.run_cycle(&client, pool, config, site, ends).await;
    }

    fn instant_backfill() -> Backfill {
        Backfill {
            pause: Duration::ZERO,
            ..Backfill::new()
        }
    }

    #[tokio::test]
    async fn backfill_saves_actuals_then_stops_and_remembers() {
        let (_dir, pool) = pool().await;
        let config = fake_amber().await;
        let mut backfill = instant_backfill();
        cycle(&mut backfill, &config, &pool, "s1").await;
        // Chunk 1 saved the fixture's one actual; chunk 2 (ending on its day)
        // got nothing older and stopped; the marker row is written.
        let rows = store::prices_between(&pool, Some("amber:s1"), 0, i64::MAX)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "actual");
        let reached = crate::db::get_cursor(&pool, &done_job("s1")).await.unwrap();
        assert!(reached.is_some());
        // Done for a day: a second cycle does not ask again.
        assert!(backfill.not_before.is_some());
    }

    #[tokio::test]
    async fn empty_and_refused_replies_stop_without_retrying() {
        for site in ["empty", "closed"] {
            let (_dir, pool) = pool().await;
            let config = fake_amber().await;
            let mut backfill = instant_backfill();
            cycle(&mut backfill, &config, &pool, site).await;
            let marker = crate::db::get_cursor(&pool, &done_job(site)).await.unwrap();
            assert!(marker.is_some(), "{site}");
            assert_eq!(backfill.failures, 0);
        }
    }

    #[tokio::test]
    async fn a_failure_backs_off_instead_of_marking_done() {
        let (_dir, pool) = pool().await;
        let mut config = fake_amber().await;
        // Nothing listens on port 9: a connection error is a passing failure.
        config.base_url = "http://127.0.0.1:9/v1".into();
        let mut backfill = instant_backfill();
        cycle(&mut backfill, &config, &pool, "s1").await;
        assert_eq!(backfill.failures, 1);
        assert!(backfill.not_before.is_some());
        assert_eq!(
            crate::db::get_cursor(&pool, &done_job("s1")).await.unwrap(),
            None
        );
    }
}
