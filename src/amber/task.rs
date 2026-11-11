//! The Amber price task: a loop inside `serve` that keeps `price_interval` up
//! to date. Read-only: it only calls `GET /sites` and `GET .../prices/current`.
//!
//! It fetches once at startup, then every 5 minutes, 15 seconds after each
//! 5-minute boundary (the current interval's price settles about 15 seconds
//! after it starts; before that Amber only has an estimate). Each fetch asks for the recent past, the current interval and
//! the forecast, and saves them with the rules in [`super::store`].
//!
//! After each good fetch, the time left before the next one is used to
//! backfill past prices a few weeks at a time (see [`super::backfill`]); the
//! backfill never delays a live fetch.
//!
//! When something fails it waits longer each time (like the iSolarCloud sync).
//! A 404 means the site is closed or the id is wrong, which retrying quickly
//! cannot fix, so then it only tries once an hour.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use anyhow::{Result, anyhow};
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{debug, error, info, warn};

use super::backfill::Backfill;
use super::client::{Client, NotFound};
use super::models::IntervalKind;
use super::probe::{SiteChoice, choose_site};
use super::store;
use crate::config::Amber;
use crate::hub::{Hub, HubEvent};

/// Amber prices change every 5 minutes.
const INTERVAL_SECS: i64 = 300;
/// How long after the boundary to ask. The current interval's price settles
/// about 15 seconds after the interval starts; asking earlier stores the
/// pre-settlement estimate.
const OFFSET_SECS: i64 = 15;
/// The first fetch after startup asks for a day of history (288 x 5 minutes),
/// to fill the gap since the last run.
const STARTUP_PREVIOUS: u32 = 288;
/// Later fetches only need the last hour, to pick up final prices that
/// replaced the current one.
const REFRESH_PREVIOUS: u32 = 12;
/// 12 x 5 minutes = the next hour of 5-minute forecast rows.
const NEXT_FIVE: u32 = 12;
/// 48 x 30 minutes = 24 hours of 30-minute forecast rows.
const NEXT_THIRTY: u32 = 48;
/// First wait after a failed fetch.
const BACKOFF_START: Duration = Duration::from_secs(30);
/// The wait after failures never grows beyond this.
const BACKOFF_CAP: Duration = Duration::from_secs(10 * 60);
/// How often to try again when Amber says 404 or there is no site to use.
const HOURLY: Duration = Duration::from_secs(3600);

/// The unix time of the next fetch after `now`: the next 5-minute boundary
/// plus [`OFFSET_SECS`]. At 12:00:03 that is 12:00:15, at 12:00:15 it is
/// 12:05:15.
fn next_fetch_at(now: i64) -> i64 {
    let this_slot = now.div_euclid(INTERVAL_SECS) * INTERVAL_SECS + OFFSET_SECS;
    if this_slot > now {
        this_slot
    } else {
        this_slot + INTERVAL_SECS
    }
}

/// How long to wait after the `failures`-th failure in a row (1-based):
/// 30 s, doubling, capped at 10 min.
pub(super) fn backoff_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (BACKOFF_START * 2u32.pow(doublings)).min(BACKOFF_CAP)
}

/// How long to wait before the next try after an error. A closed or unknown
/// site (404) is tried hourly; anything else backs off.
fn retry_wait(err: &anyhow::Error, failures: u32) -> Duration {
    if is_site_problem(err) {
        HOURLY
    } else {
        backoff_delay(failures)
    }
}

/// Errors that waiting a few minutes will not fix.
fn is_site_problem(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotFound>().is_some() || err.downcast_ref::<NoSite>().is_some()
}

/// There is no single site to use: none on the account, or several and
/// `site_id` is not set.
#[derive(Debug)]
struct NoSite(String);

impl std::fmt::Display for NoSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoSite {}

/// Finds the site id: the configured one, or the account's only site.
async fn resolve_site(client: &Client, config: &Amber) -> Result<String> {
    if let Some(id) = &config.site_id {
        return Ok(id.clone());
    }
    let sites = client.sites().await?.items;
    match choose_site(&sites, None) {
        SiteChoice::Use(id) => Ok(id),
        SiteChoice::NeedsChoice => Err(anyhow!(NoSite(format!(
            "the Amber account has {} sites: set site_id in [amber] to the one to use",
            sites.len()
        )))),
        SiteChoice::NoSites => Err(anyhow!(NoSite(
            "the Amber account has no sites".to_string()
        ))),
    }
}

/// One fetch: asks Amber for prices and saves them, then prunes old
/// forecasts. Returns how many intervals were saved.
///
/// Two calls, because Amber answers in one interval length at a time:
/// 30-minute rows for the 24 h forecast, then 5-minute rows for the past, the
/// current interval and the next hour. The 5-minute call goes second on
/// purpose: a row is keyed by its start, so where both lengths start at the
/// same time the 5-minute row (the more precise one) is the one that stays.
/// If either call fails, the whole fetch fails and is retried with backoff.
async fn fetch_and_store(
    client: &Client,
    pool: &SqlitePool,
    site_id: &str,
    previous: u32,
    hub: &Hub,
) -> Result<usize> {
    let long = client.current_prices(site_id, 0, NEXT_THIRTY, 30).await?;
    let short = client
        .current_prices(site_id, previous, NEXT_FIVE, 5)
        .await?;
    let now = Utc::now().timestamp();
    let source = store::source_name(site_id);
    // From the 30-minute call only the forecast is wanted: its "current" row
    // is the whole half hour under way, which the 5-minute rows describe better.
    let forecasts: Vec<_> = long
        .items
        .into_iter()
        .filter(|item| item.kind == IntervalKind::ForecastInterval)
        .collect();
    let mut saved = store::save_prices(pool, &source, &forecasts, now).await?;
    saved += store::save_prices(pool, &source, &short.items, now).await?;
    hub.publish(HubEvent::PricesStored { source });
    let pruned = store::prune_forecasts(pool, now).await?;
    debug!(saved, pruned, "Amber prices stored");
    Ok(saved)
}

/// Keeps the prices up to date until `shutdown` completes.
///
/// Amber trouble never ends the loop: it is logged and retried later. Only a
/// bad setting (the HTTP client cannot be built) returns `Err`, before
/// anything starts.
pub async fn run(
    config: Amber,
    pool: SqlitePool,
    hub: &Hub,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let client = Client::new(&config)?;
    tokio::pin!(shutdown);
    info!("Amber price task started");

    let mut site_id: Option<String> = None;
    let mut previous = STARTUP_PREVIOUS;
    let mut failures = 0u32;
    let mut backfill = Backfill::new();
    loop {
        // Stopping in the middle of a fetch is safe: rows are saved in one
        // transaction, so there is never half a reply in the database.
        let result = tokio::select! {
            _ = &mut shutdown => break,
            result = async {
                if site_id.is_none() {
                    site_id = Some(resolve_site(&client, &config).await?);
                }
                let id = site_id.as_deref().expect("set just above");
                fetch_and_store(&client, &pool, id, previous, hub).await
            } => result,
        };

        let now = Utc::now().timestamp();
        let wait = match result {
            Ok(saved) => {
                // The first good fetch is logged at info, so a normal log shows
                // the task is working. The 5-minute refreshes after it stay at
                // debug to keep the log quiet.
                let first = previous == STARTUP_PREVIOUS;
                if first {
                    info!(
                        site = site_id.as_deref().unwrap_or(""),
                        saved, "Amber prices fetched; refreshing every 5 minutes"
                    );
                } else if failures > 0 {
                    info!(saved, "Amber prices are back");
                } else {
                    debug!(saved, "Amber prices updated");
                }
                failures = 0;
                previous = REFRESH_PREVIOUS;
                // Fill in history with the time left before the next live
                // fetch. It is cut short on shutdown, like a fetch.
                let wait = Duration::from_secs((next_fetch_at(now) - now).max(0) as u64);
                if let Some(id) = site_id.as_deref() {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = backfill.run_cycle(&client, &pool, &config, id, Instant::now() + wait) => {}
                    }
                }
                // The backfill used some of that time: wait for what is left.
                let now = Utc::now().timestamp();
                Duration::from_secs((next_fetch_at(now) - now).max(0) as u64)
            }
            Err(err) => {
                failures = failures.saturating_add(1);
                let wait = retry_wait(&err, failures);
                if err.downcast_ref::<NotFound>().is_some() {
                    error!(
                        "Amber answered 404 for the site: it is closed or site_id is wrong. \
                         Prices will not update; trying again in an hour ({err:#})"
                    );
                } else if is_site_problem(&err) {
                    error!("{err:#}; trying again in an hour");
                } else {
                    warn!(
                        "Amber prices failed: {err:#}; trying again in {}s",
                        wait.as_secs()
                    );
                }
                wait
            }
        };
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    info!("Amber price task stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;

    #[test]
    fn fetches_fifteen_seconds_after_each_five_minute_boundary() {
        let base = 1_800_000_000; // a multiple of 300, so a 5-minute boundary
        assert_eq!(OFFSET_SECS, 15);
        assert_eq!(next_fetch_at(base), base + 15);
        assert_eq!(next_fetch_at(base + 3), base + 15);
        // At or after the +15 mark, wait for the next boundary.
        assert_eq!(next_fetch_at(base + 15), base + 315);
        assert_eq!(next_fetch_at(base + 299), base + 315);
        assert_eq!(next_fetch_at(base - 1), base + 15);
        // Always in the future, and never more than 5 minutes away.
        for now in base..base + 900 {
            let next = next_fetch_at(now);
            assert!(next > now && next - now <= 300, "{now}");
            assert_eq!((next - OFFSET_SECS) % 300, 0);
        }
    }

    #[test]
    fn backoff_doubles_to_the_cap() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(secs, [30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(backoff_delay(0).as_secs(), 30);
        assert_eq!(backoff_delay(u32::MAX).as_secs(), 600);
    }

    #[test]
    fn a_missing_site_is_retried_hourly_other_errors_back_off() {
        let gone = anyhow::Error::new(NotFound("/sites/x/prices/current".into()));
        assert_eq!(retry_wait(&gone, 1), HOURLY);
        assert_eq!(retry_wait(&gone, 9), HOURLY);
        let no_site = anyhow!(NoSite("no sites".into()));
        assert_eq!(retry_wait(&no_site, 1), HOURLY);
        let other = anyhow!("connection reset");
        assert_eq!(retry_wait(&other, 1), Duration::from_secs(30));
        assert_eq!(retry_wait(&other, 3), Duration::from_secs(120));
    }

    /// A local server standing in for Amber: site `s1` answers with the test
    /// prices, `gone` is a closed site (404).
    async fn fake_amber() -> Amber {
        let app = Router::new()
            .route(
                "/v1/sites",
                get(|| async { include_str!("testdata/sites.json") }),
            )
            .route(
                "/v1/sites/s1/prices/current",
                get(|| async { include_str!("testdata/prices_current.json") }),
            )
            .route(
                "/v1/sites/gone/prices/current",
                get(|| async { (StatusCode::NOT_FOUND, "{}") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Amber {
            token: "t".to_string(),
            site_id: Some("s1".to_string()),
            backfill_from: None,
            base_url: format!("http://{addr}/v1"),
            compare: Default::default(),
        }
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = crate::db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn a_fetch_saves_prices_and_a_closed_site_is_a_not_found() {
        let (_dir, pool) = pool().await;
        let config = fake_amber().await;
        let client = Client::new(&config).unwrap();

        let saved = fetch_and_store(&client, &pool, "s1", 12, &Hub::default())
            .await
            .unwrap();
        // One forecast from the 30-minute call plus the three 5-minute rows.
        assert_eq!(saved, 4);
        let stored = store::prices_between(&pool, Some("amber:s1"), 0, i64::MAX)
            .await
            .unwrap();
        // The fixture's forecast is from 2021, so the prune after the save
        // removes it; the actual and the current interval stay.
        assert_eq!(stored.len(), 2);

        let err = fetch_and_store(&client, &pool, "gone", 12, &Hub::default())
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<NotFound>().is_some(), "{err:#}");
        assert_eq!(retry_wait(&err, 1), HOURLY);
    }

    #[tokio::test]
    async fn the_only_site_is_used_when_site_id_is_not_set() {
        let mut config = fake_amber().await;
        config.site_id = None;
        let client = Client::new(&config).unwrap();
        // The test account has two sites, so the user must choose.
        let err = resolve_site(&client, &config).await.unwrap_err();
        assert!(is_site_problem(&err), "{err:#}");
        config.site_id = Some("x".into());
        assert_eq!(resolve_site(&client, &config).await.unwrap(), "x");
    }

    #[tokio::test]
    async fn the_task_fetches_at_start_and_stops_on_shutdown() {
        let (_dir, pool) = pool().await;
        let config = fake_amber().await;
        let stop = tokio::time::sleep(Duration::from_millis(500));
        tokio::time::timeout(
            Duration::from_secs(5),
            run(config, pool.clone(), &Hub::default(), stop),
        )
        .await
        .expect("did not stop on shutdown")
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM price_interval")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2); // the 2021 forecast was pruned
    }
}
