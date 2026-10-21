//! Downloading history from iSolarCloud into SQLite.
//!
//! The parsing and time helpers are pure functions (no network, no database)
//! so they can be unit tested. [`sync_range`] ties them together, and is used
//! by both [`backfill`] and the live loop, [`run_live_sync`].

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone};
use chrono_tz::Tz;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::config::IsolarCloud;
use crate::db::{self, Point, Reading};
use crate::isolar::client::{ApiError, Client};

const HISTORY_ENDPOINT: &str = "/openapi/getDevicePointMinuteDataList";
const POINT_INFO_ENDPOINT: &str = "/openapi/getOpenPointInfo";
/// Names of the jobs in the `sync_state` table. Each keeps its own cursor,
/// so `backfill` can run while `serve` is live-syncing.
pub const BACKFILL_JOB: &str = "backfill";
pub const LIVE_JOB: &str = "live";
/// Where `serve`'s background backfill stops: the live sync covers the rest.
/// Saved once, the first time the background backfill runs.
const BACKFILL_UNTIL_JOB: &str = "backfill_until";

/// The history has one row every 5 minutes.
const STEP_SECS: i64 = 5 * 60;
/// First window size to try. It is halved if the API rejects it.
/// The real API rejected 24, 12 and 6 hours but accepted 3, so start there.
const START_WINDOW_SECS: i64 = 3 * 3600;
/// One hour is confirmed to work, so never go below it.
const MIN_WINDOW_SECS: i64 = 3600;

// The API allows 2,000 calls per hour and 100,000 per month; stay below both.
// The hourly margin covers logins, retries and our hour not lining up with
// iSolarCloud's.
const MAX_CALLS_PER_HOUR: i64 = 1800;
const MAX_CALLS_PER_30_DAYS: i64 = 80_000;
const BUDGET_RECHECK: Duration = Duration::from_secs(60);
/// Pause between calls, to be polite to the server.
const CALL_PAUSE: Duration = Duration::from_secs(1);
/// How often the live loop prunes old `api_call` rows.
const PRUNE_EVERY_SECS: i64 = 24 * 3600;
/// History the background backfill fetches per 5-minute tick: 18 days is
/// about 144 calls, three to four minutes, so the live sync keeps its rhythm.
/// At 12 ticks an hour that is about 1,730 calls with the live sync, just under
/// the hourly cap, and three years of history fill in within about 5 hours.
const BACKFILL_STEP_SECS: i64 = 18 * 24 * 3600;

/// Data lands a little after each 5-minute mark, so live sync waits this long.
const TICK_LAG_SECS: i64 = 60;
/// A cursor further behind than this is logged as "catching up".
const CATCH_UP_SECS: i64 = 2 * 24 * 3600;

/// The timestamp format used by the history API, in the plant's local time.
const LOCAL_FORMAT: &str = "%Y%m%d%H%M%S";

/// Unix seconds (UTC) to the API's local-time text, e.g. "20261020110000".
pub fn format_local(ts_utc: i64, tz: Tz) -> String {
    DateTime::from_timestamp(ts_utc, 0)
        .expect("timestamp out of range")
        .with_timezone(&tz)
        .format(LOCAL_FORMAT)
        .to_string()
}

/// The API's local-time text to unix seconds (UTC).
///
/// Returns `None` if the text is malformed or the local time never happened
/// (the hour skipped when the clocks go forward). A local time that happened
/// twice (the hour repeated when the clocks go back) gives the earlier one.
pub fn parse_local(s: &str, tz: Tz) -> Option<i64> {
    let naive = NaiveDateTime::parse_from_str(s, LOCAL_FORMAT).ok()?;
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(earliest, _latest) => Some(earliest.timestamp()),
        LocalResult::None => None,
    }
}

/// Like [`parse_local`], but a local time that happened twice is resolved with
/// the UTC offset in force at `near_ts` (normally the start of the window the
/// text came from). A window never crosses a clock change (see
/// [`next_window_end`]), so this picks the right one of the two copies.
pub fn parse_local_near(s: &str, tz: Tz, near_ts: i64) -> Option<i64> {
    let naive = NaiveDateTime::parse_from_str(s, LOCAL_FORMAT).ok()?;
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(earliest, latest) => {
            let want = offset_secs(near_ts, tz);
            if latest.offset().fix().local_minus_utc() == want
                && earliest.offset().fix().local_minus_utc() != want
            {
                Some(latest.timestamp())
            } else {
                Some(earliest.timestamp())
            }
        }
        LocalResult::None => None,
    }
}

/// The UTC offset, in seconds, in force at a unix timestamp.
fn offset_secs(ts_utc: i64, tz: Tz) -> i32 {
    let t = DateTime::from_timestamp(ts_utc, 0).expect("timestamp out of range");
    tz.offset_from_utc_datetime(&t.naive_utc())
        .fix()
        .local_minus_utc()
}

/// End (exclusive) of the window that starts at `start`: at most `step` long,
/// at most `end`, and never past a clock change. Every instant the window asks
/// for (up to one step before its end) then shares one UTC offset, so the
/// local start and end strings are in order and unambiguous.
pub fn next_window_end(start: i64, end: i64, step: i64, tz: Tz) -> i64 {
    let to = (start + step).min(end);
    let base = offset_secs(start, tz);
    if offset_secs(to - STEP_SECS, tz) == base {
        return to;
    }
    // Bisect for the first instant whose offset differs from the start's.
    // Invariant: `lo` has the start's offset, `hi` does not.
    let (mut lo, mut hi) = (start, to - STEP_SECS);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if offset_secs(mid, tz) == base {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// "p83022" becomes 83022. Anything else (such as "time_stamp") gives `None`.
fn point_id_from_key(key: &str) -> Option<i64> {
    key.strip_prefix('p')?.parse().ok()
}

/// The API sends numbers as strings, and uses "", "--" or null for no value.
fn parse_value(value: &Value) -> Option<f64> {
    match value {
        Value::String(s) => s.trim().parse().ok(), // "" and "--" fail to parse
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Turns the history `result_data` into readings for one device (`ps_key`).
/// Rows whose local time does not exist are skipped. Points missing from a
/// row are simply not stored; points present but empty are stored as `None`.
/// `window_start` (unix seconds) is where the window began; it decides which
/// copy of a repeated local time (clocks going back) is meant.
pub fn parse_history(result_data: &Value, ps_key: &str, tz: Tz, window_start: i64) -> Vec<Reading> {
    let mut readings = Vec::new();
    let Some(rows) = result_data[ps_key].as_array() else {
        return readings; // no data for this device in the window
    };
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let Some(ts) = object
            .get("time_stamp")
            .and_then(Value::as_str)
            .and_then(|s| parse_local_near(s, tz, window_start))
        else {
            debug!(?row, "skipping row with a missing or invalid time_stamp");
            continue;
        };
        for (key, value) in object {
            if let Some(point_id) = point_id_from_key(key) {
                readings.push(Reading {
                    point_id,
                    ts,
                    value: parse_value(value),
                });
            }
        }
    }
    readings
}

/// Reads the point list from `getOpenPointInfo` (`result_data.pageList`).
fn parse_points(data: &Value) -> Vec<Point> {
    let list = data["pageList"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    list.iter()
        .filter_map(|item| {
            let point_id = item["point_id"].as_i64()?;
            Some(Point {
                point_id,
                code: Some(format!("p{point_id}")),
                name: item["point_name"].as_str().map(str::to_string),
                unit: item["storage_unit"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// What to do before the next API call, given how many calls were recently made.
#[derive(Debug, PartialEq)]
enum Budget {
    Go,
    /// Too many calls in the last hour: wait and check again.
    WaitForHour,
    /// Too many calls in the last 30 days: give up.
    MonthlyLimit,
}

fn check_budget(calls_last_hour: i64, calls_last_30_days: i64) -> Budget {
    if calls_last_30_days >= MAX_CALLS_PER_30_DAYS {
        Budget::MonthlyLimit
    } else if calls_last_hour >= MAX_CALLS_PER_HOUR {
        Budget::WaitForHour
    } else {
        Budget::Go
    }
}

/// Returns once another call is allowed. Fails if the monthly budget is spent.
async fn wait_for_budget(pool: &SqlitePool) -> Result<()> {
    loop {
        let now = db::now();
        let hour = db::api_calls_since(pool, now - 3600).await?;
        let month = db::api_calls_since(pool, now - 30 * 24 * 3600).await?;
        match check_budget(hour, month) {
            Budget::Go => return Ok(()),
            Budget::WaitForHour => {
                info!(hour, "hourly API budget used up; waiting");
                tokio::time::sleep(BUDGET_RECHECK).await;
            }
            Budget::MonthlyLimit => bail!(
                "stopping: {month} API calls in the last 30 days (limit {MAX_CALLS_PER_30_DAYS}). \
                 Run backfill again later to resume"
            ),
        }
    }
}

/// Makes one API call with the budget check, and records it in `api_call`.
async fn budgeted_call(
    client: &Client,
    pool: &SqlitePool,
    path: &str,
    params: Value,
) -> Result<Value> {
    wait_for_budget(pool).await?;
    let result = client.call(path, params).await;
    db::record_api_call(pool, path, result.is_ok()).await?;
    tokio::time::sleep(CALL_PAUSE).await;
    result
}

/// Makes sure every wanted point has a row, because readings refer to it.
/// A point that already has a row (maybe with its real name) is left alone.
pub async fn ensure_points(pool: &SqlitePool, wanted: &[i64]) -> Result<()> {
    for &point_id in wanted {
        let point = Point {
            point_id,
            code: Some(format!("p{point_id}")),
            name: None,
            unit: None,
        };
        db::insert_point_if_missing(pool, &point).await?;
    }
    Ok(())
}

/// Saves the metadata (name, unit) of every point, so readings have a point
/// row to refer to. Runs once at the start of each backfill.
async fn sync_points(
    client: &Client,
    pool: &SqlitePool,
    wanted: &[i64],
    ps_key: &str,
) -> Result<()> {
    // Placeholders first, so a wanted point missing from the API's list still
    // works. The real metadata below overwrites them.
    ensure_points(pool, wanted).await?;

    let size = 200;
    let mut page = 1;
    let mut seen = 0;
    loop {
        let params = json!({ "device_type": "11", "type": "2", "curPage": page, "size": size });
        let data = budgeted_call(client, pool, POINT_INFO_ENDPOINT, params)
            .await
            .with_context(|| format!("could not fetch point info for {ps_key}"))?;
        let points = parse_points(&data);
        for point in &points {
            db::upsert_point(pool, point).await?;
        }
        seen += points.len() as i64;
        let total = data["rowCount"].as_i64().unwrap_or(0);
        if points.is_empty() || seen >= total {
            info!(points = seen, "point metadata saved");
            return Ok(());
        }
        page += 1;
    }
}

/// The plant settings from the config, checked and ready to use.
struct PlantSetup {
    plant_id: i64,
    tz: Tz,
    /// Device key used by the history API, e.g. "1204811_11_0_0".
    ps_key: String,
    /// The configured points as the API wants them, e.g. "p83022,p83033".
    points: String,
    /// Local midnight of `history_from`, where `serve`'s backfill starts.
    history_from: Option<i64>,
}

impl PlantSetup {
    fn from_config(config: &IsolarCloud) -> Result<Self> {
        let plant_id = config.plant_id.ok_or_else(|| {
            anyhow!("syncing needs plant_id in config.toml (run login-test to see your plants)")
        })?;
        let tz: Tz = config
            .timezone
            .parse()
            .map_err(|e| anyhow!("invalid timezone {:?}: {e}", config.timezone))?;
        let points = config
            .points
            .iter()
            .map(|id| format!("p{id}"))
            .collect::<Vec<_>>()
            .join(",");
        let history_from = config
            .history_from
            .as_deref()
            .map(|text| {
                let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| {
                    anyhow!("history_from must look like \"2023-02-21\", got {text:?}")
                })?;
                parse_local(&date.format("%Y%m%d000000").to_string(), tz)
                    .ok_or_else(|| anyhow!("{date} has no local midnight in {tz}"))
            })
            .transpose()?;
        Ok(Self {
            plant_id,
            tz,
            ps_key: format!("{plant_id}_11_0_0"),
            points,
            history_from,
        })
    }
}

/// What one [`sync_range`] call did.
#[derive(Debug, Default, PartialEq)]
pub struct SyncStats {
    /// Readings saved.
    pub rows: usize,
    /// History API calls made (including ones the API rejected).
    pub calls: u32,
}

/// Downloads history for `start_ts..end_ts` (end exclusive, unix seconds) in
/// windows, saving `job`'s cursor after each one. Used by both `backfill` and
/// the live sync. Safe to stop and run again.
pub async fn sync_range(
    client: &Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
    job: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<SyncStats> {
    let plant = PlantSetup::from_config(config)?;
    let tz = plant.tz;
    let mut stats = SyncStats::default();
    let mut start = start_ts;
    let mut window = START_WINDOW_SECS;
    while start < end_ts {
        // `window_end` is exclusive. The API includes its end time, so we ask
        // for one step less; the next window then starts right after.
        let window_end = next_window_end(start, end_ts, window, tz);
        let params = json!({
            "ps_key_list": [&plant.ps_key],
            "points": plant.points,
            "start_time_stamp": format_local(start, tz),
            "end_time_stamp": format_local(window_end - STEP_SECS, tz),
            "minute_interval": "5",
        });
        debug!(window_hours = window / 3600, "requesting window");

        stats.calls += 1;
        let data = match budgeted_call(client, pool, HISTORY_ENDPOINT, params).await {
            Ok(data) => data,
            // Only an error from iSolarCloud itself suggests the window was
            // too big. Network errors are returned as they are.
            Err(err) if err.downcast_ref::<ApiError>().is_some() && window > MIN_WINDOW_SECS => {
                window = (window / 2).max(MIN_WINDOW_SECS);
                warn!(
                    window_hours = window / 3600,
                    "API rejected the window ({err:#}); retrying smaller"
                );
                continue;
            }
            Err(err) => return Err(err),
        };

        let readings = parse_history(&data, &plant.ps_key, tz, start);
        db::upsert_readings(pool, plant.plant_id, &readings).await?;
        db::set_cursor(pool, job, window_end).await?;
        stats.rows += readings.len();

        let calls_this_hour = db::api_calls_since(pool, db::now() - 3600).await?;
        info!(
            from = %format_local(start, tz),
            to = %format_local(window_end, tz),
            rows = readings.len(),
            calls_this_hour,
            window_hours = window / 3600,
            "window saved"
        );
        start = window_end;
    }
    Ok(stats)
}

/// Downloads 5-minute history from local midnight of `from` until now.
/// Safe to stop and run again: it resumes from the saved cursor.
pub async fn backfill(
    client: &Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
    from: NaiveDate,
) -> Result<()> {
    let plant = PlantSetup::from_config(config)?;
    let tz = plant.tz;

    let from_ts = parse_local(&from.format("%Y%m%d000000").to_string(), tz)
        .ok_or_else(|| anyhow!("{from} has no local midnight in {tz}"))?;
    let cursor = db::get_cursor(pool, BACKFILL_JOB).await?;
    let start = cursor.map_or(from_ts, |c| c.max(from_ts));
    let end = round_down_to_step(db::now());
    if start >= end {
        info!("history is already up to date");
        return Ok(());
    }
    match cursor {
        Some(c) if c >= from_ts => info!(resume_at = %format_local(c, tz), "resuming backfill"),
        _ => info!(start = %format_local(start, tz), "starting backfill"),
    }

    sync_points(client, pool, &config.points, &plant.ps_key).await?;
    sync_range(client, pool, config, BACKFILL_JOB, start, end).await?;
    info!("backfill finished");
    Ok(())
}

/// Rounds a timestamp down to a multiple of 5 minutes.
fn round_down_to_step(ts: i64) -> i64 {
    ts - ts % STEP_SECS
}

/// Seconds to sleep so the next live sync lands just after a 5-minute mark.
/// Data shows up a little after each mark, so we wait an extra minute.
pub fn next_tick_delay(now_ts: i64) -> Duration {
    // The inner `%` makes an exact 5-minute mark give 0 rather than 300.
    let to_boundary = (STEP_SECS - now_ts % STEP_SECS) % STEP_SECS;
    Duration::from_secs((to_boundary + TICK_LAG_SECS) as u64)
}

/// Unix seconds of the most recent local midnight at or before `now_ts`.
fn local_midnight(now_ts: i64, tz: Tz) -> Option<i64> {
    let today = DateTime::from_timestamp(now_ts, 0)?
        .with_timezone(&tz)
        .date_naive();
    parse_local(&today.format("%Y%m%d000000").to_string(), tz)
}

/// How far back each live sync re-reads before the cursor. iSolarCloud can
/// publish the latest readings a few minutes late, and the cursor has already
/// moved past them; re-reading is safe because writes are upserts.
const LIVE_OVERLAP_SECS: i64 = 15 * 60;

/// Where the next live sync should start: a little before its saved cursor,
/// or else local midnight today.
async fn live_start(pool: &SqlitePool, tz: Tz, now_ts: i64) -> Result<i64> {
    let live = db::get_cursor(pool, LIVE_JOB).await?;
    let backfill = db::get_cursor(pool, BACKFILL_JOB).await?;
    let midnight = local_midnight(now_ts, tz)
        .ok_or_else(|| anyhow!("could not work out local midnight in {tz}"))?;
    if live.is_none() {
        info!(
            "no live sync yet; starting from midnight today. Run backfill to fetch older history"
        );
    }
    Ok(pick_live_start(live, backfill, midnight, now_ts))
}

/// The decision behind [`live_start`], kept pure for testing. Databases from
/// before the live sync had its own cursor only have the backfill cursor,
/// which then marked where the live sync got to. A recent one is used to carry
/// on from there; an old one is a backfill still in progress, which the live
/// sync must not chase.
fn pick_live_start(live: Option<i64>, backfill: Option<i64>, midnight: i64, now_ts: i64) -> i64 {
    match (live, backfill) {
        (Some(cursor), _) => cursor - LIVE_OVERLAP_SECS,
        (None, Some(cursor)) if now_ts - cursor <= CATCH_UP_SECS => cursor - LIVE_OVERLAP_SECS,
        _ => midnight,
    }
}

/// One live sync: from the cursor up to now (rounded down to 5 minutes).
async fn sync_once(client: &Client, pool: &SqlitePool, config: &IsolarCloud) -> Result<()> {
    let tz = PlantSetup::from_config(config)?.tz;
    let now = db::now();
    let end = round_down_to_step(now);
    let start = live_start(pool, tz, now).await?;
    if start >= end {
        debug!("already up to date");
        return Ok(());
    }
    if end - start > CATCH_UP_SECS {
        info!(since = %format_local(start, tz), "cursor is far behind; catching up");
    }
    let stats = sync_range(client, pool, config, LIVE_JOB, start, end).await?;
    info!(rows = stats.rows, calls = stats.calls, "sync done");
    Ok(())
}

/// Where `serve`'s background backfill picks up: its saved cursor, but never
/// before `history_from`. A cursor alone (from an earlier `backfill` run) is
/// resumed; with neither there is nothing to backfill.
fn backfill_start(cursor: Option<i64>, history_from: Option<i64>) -> Option<i64> {
    match (cursor, history_from) {
        (Some(c), Some(from)) => Some(c.max(from)),
        (Some(c), None) => Some(c),
        (None, from) => from,
    }
}

/// One step of `serve`'s background backfill: up to [`BACKFILL_STEP_SECS`] of
/// history, from the backfill cursor towards where the live sync began.
/// Does nothing once history is complete, so it is cheap to call every tick.
async fn backfill_step(client: &Client, pool: &SqlitePool, config: &IsolarCloud) -> Result<()> {
    let plant = PlantSetup::from_config(config)?;
    let cursor = db::get_cursor(pool, BACKFILL_JOB).await?;
    let Some(start) = backfill_start(cursor, plant.history_from) else {
        return Ok(());
    };
    let until = match db::get_cursor(pool, BACKFILL_UNTIL_JOB).await? {
        Some(until) => until,
        None => {
            // The live sync starts at a local midnight and only moves forward,
            // so it has everything since midnight today (or since its cursor,
            // if that is still earlier).
            let Some(live) = db::get_cursor(pool, LIVE_JOB).await? else {
                return Ok(());
            };
            let midnight = local_midnight(db::now(), plant.tz)
                .ok_or_else(|| anyhow!("could not work out local midnight in {}", plant.tz))?;
            let until = live.min(midnight);
            db::set_cursor(pool, BACKFILL_UNTIL_JOB, until).await?;
            until
        }
    };
    if start >= until {
        return Ok(());
    }
    let end = (start + BACKFILL_STEP_SECS).min(until);
    info!(
        from = %format_local(start, plant.tz),
        to = %format_local(end, plant.tz),
        "filling in history"
    );
    sync_range(client, pool, config, BACKFILL_JOB, start, end).await?;
    if end == until {
        info!("history is complete");
    }
    Ok(())
}

/// A sync that logs in first if there is no token yet, and logs in again and
/// retries once if iSolarCloud reports an error. The token-expiry codes are not
/// confirmed, so any `ApiError` gets one retry.
async fn sync_with_relogin(
    client: &mut Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
) -> Result<()> {
    if !client.is_logged_in() {
        client.login().await?;
    }
    match sync_once(client, pool, config).await {
        Err(err) if err.downcast_ref::<ApiError>().is_some() => {
            warn!("iSolarCloud returned an error ({err:#}); logging in again");
            client.login().await?;
            sync_once(client, pool, config).await
        }
        other => other,
    }
}

/// First wait after a failed attempt to reach iSolarCloud.
const BACKOFF_START: Duration = Duration::from_secs(60);
/// The wait never grows beyond this.
const BACKOFF_CAP: Duration = Duration::from_secs(30 * 60);

/// How long to wait after the `failures`-th failure in a row (1-based):
/// 1 min, then doubling, capped at 30 min.
fn backoff_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (BACKOFF_START * 2u32.pow(doublings)).min(BACKOFF_CAP)
}

/// Checks the sync settings without touching the network, so `serve` can fail
/// fast on a config mistake that no retry would fix.
pub fn check_sync_config(config: &IsolarCloud) -> Result<()> {
    PlantSetup::from_config(config).map(|_| ())
}

/// Keeps the database up to date until `shutdown` completes.
///
/// Cloud trouble never ends the loop: a failed login or sync is logged and
/// retried (logins with [`backoff_delay`]). Only a setting that can never work
/// (see [`check_sync_config`]) or a database error while setting up returns
/// `Err`, before any network call.
///
/// The loop takes the `Client` by value because `login` needs `&mut self`.
/// Nothing else uses the client while we sync, so owning it is simpler than
/// sharing it behind a lock.
pub async fn run_live_sync(
    mut client: Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // Readings need point rows. Check the config now, so mistakes fail at startup.
    PlantSetup::from_config(config)?;
    ensure_points(pool, &config.points).await?;

    // A future can only be awaited by value, but we wait on it in every loop
    // round. Pinning lets `select!` poll it by `&mut` without moving it.
    tokio::pin!(shutdown);
    info!("live sync started");
    let mut last_prune = 0;
    // Failed attempts to log in since the last success.
    let mut login_failures: u32 = 0;
    loop {
        // Once at startup, then about once a day.
        if db::now() - last_prune >= PRUNE_EVERY_SECS {
            last_prune = db::now();
            match db::prune_api_calls(pool).await {
                Ok(rows) => info!(rows, "pruned old API call records"),
                Err(err) => warn!("pruning API calls failed: {err:#}"),
            }
        }
        // Dropping an unfinished sync is safe: each window is saved
        // atomically, along with its cursor.
        let logged_in_before = client.is_logged_in();
        tokio::select! {
            _ = &mut shutdown => break,
            result = sync_with_relogin(&mut client, pool, config) => {
                // A failed tick is not fatal: try again on the next one.
                if let Err(err) = result {
                    if !client.is_logged_in() {
                        // Not logged in: iSolarCloud is down or slow, or the
                        // login is wrong. Back off instead of hammering it.
                        login_failures += 1;
                        let wait = backoff_delay(login_failures);
                        warn!(
                            "login to iSolarCloud failed ({err:#}); retrying in {}s",
                            wait.as_secs()
                        );
                        tokio::select! {
                            _ = &mut shutdown => break,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        continue;
                    }
                    warn!("sync failed: {err:#}");
                }
            }
        }
        if !logged_in_before && client.is_logged_in() {
            info!("logged in to iSolarCloud");
        }
        login_failures = 0;

        // Then a slice of older history, if any is missing. A failure is
        // retried on the next tick; the cursor keeps what was saved.
        tokio::select! {
            _ = &mut shutdown => break,
            result = backfill_step(&client, pool, config) => {
                if let Err(err) = result {
                    warn!("backfill failed: {err:#}");
                }
            }
        }

        let delay = next_tick_delay(db::now());
        debug!(seconds = delay.as_secs(), "sleeping until next sync");
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    info!("sync stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff_delay(n).as_secs()).collect();
        assert_eq!(secs, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
        assert_eq!(backoff_delay(0).as_secs(), 60);
        assert_eq!(backoff_delay(u32::MAX).as_secs(), 1800);
    }

    /// A config whose logins fail at once: nothing listens on port 1.
    fn offline_config(history_from: Option<&str>) -> IsolarCloud {
        IsolarCloud {
            gateway: "http://127.0.0.1:1".into(),
            appkey: "k".into(),
            access_key: "a".into(),
            rsa_public_key: String::new(),
            encrypt: false,
            username: "u".into(),
            password: "p".into(),
            plant_id: Some(1),
            points: vec![83022],
            timezone: "Australia/Adelaide".into(),
            history_from: history_from.map(Into::into),
        }
    }

    #[test]
    fn history_from_must_be_a_date() {
        assert!(check_sync_config(&offline_config(Some("2023-02-21"))).is_ok());
        let err = check_sync_config(&offline_config(Some("21/02/2023"))).unwrap_err();
        assert!(err.to_string().contains("history_from"), "{err}");
    }

    #[tokio::test]
    async fn backfill_step_stops_where_the_live_sync_began() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let client = Client::new(offline_config(None)).unwrap();
        let now = db::now();

        // Nothing configured and no cursor: nothing to do, nothing saved.
        db::set_cursor(&pool, LIVE_JOB, now).await.unwrap();
        backfill_step(&client, &pool, &offline_config(None))
            .await
            .unwrap();
        assert_eq!(
            db::get_cursor(&pool, BACKFILL_UNTIL_JOB).await.unwrap(),
            None
        );

        // History already reaches the live sync: the end point is saved (at
        // most midnight today) and no call is made, which would fail here.
        db::set_cursor(&pool, BACKFILL_JOB, now).await.unwrap();
        let config = offline_config(Some("2023-02-21"));
        backfill_step(&client, &pool, &config).await.unwrap();
        let until = db::get_cursor(&pool, BACKFILL_UNTIL_JOB).await.unwrap();
        let midnight = local_midnight(now, PlantSetup::from_config(&config).unwrap().tz);
        assert_eq!(until, midnight);
    }

    #[tokio::test]
    async fn live_sync_survives_a_failing_login_and_stops_on_shutdown() {
        let dir = std::env::temp_dir().join(format!("solar-dash-test-{}", std::process::id()));
        let path = dir.join("test.db");
        let pool = db::open(path.to_str().unwrap()).await.unwrap();
        let config = offline_config(None);
        let client = Client::new(config.clone()).unwrap();
        // The first backoff is a minute; shutdown must cut it short.
        let shutdown = tokio::time::sleep(Duration::from_millis(500));
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_live_sync(client, &pool, &config, shutdown),
        )
        .await;
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(result.expect("did not stop promptly on shutdown").is_ok());
    }

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;
    const KEY: &str = "1204811_11_0_0";

    #[test]
    fn parses_point_keys() {
        assert_eq!(point_id_from_key("p83022"), Some(83022));
        assert_eq!(point_id_from_key("time_stamp"), None);
        assert_eq!(point_id_from_key("p"), None);
        assert_eq!(point_id_from_key("pabc"), None);
        assert_eq!(point_id_from_key("83022"), None);
    }

    #[test]
    fn parses_history_sample() {
        let data = json!({ KEY: [
            { "p83022": "5500.0", "p83033": "2585.0", "time_stamp": "20261020110000" },
            { "p83022": "--", "p83033": "", "time_stamp": "20261020110500" },
            { "p83022": null, "time_stamp": "20261020111000", "other": "x" },
        ]});
        let readings = parse_history(&data, KEY, ADELAIDE, 0);

        // 11:00 local on 20 Oct is ACDT (UTC+10:30).
        let t0 = parse_local("20261020110000", ADELAIDE).unwrap();
        assert_eq!(t0, 1_792_456_200); // 2026-10-20 00:30:00 UTC
        let at = |ts: i64, id: i64| {
            readings
                .iter()
                .find(|r| r.ts == ts && r.point_id == id)
                .map(|r| r.value)
        };
        assert_eq!(at(t0, 83022), Some(Some(5500.0)));
        assert_eq!(at(t0, 83033), Some(Some(2585.0)));
        assert_eq!(at(t0 + 300, 83022), Some(None)); // "--"
        assert_eq!(at(t0 + 300, 83033), Some(None)); // ""
        assert_eq!(at(t0 + 600, 83022), Some(None)); // null
        assert_eq!(at(t0 + 600, 83033), None); // missing key: nothing stored
        assert_eq!(readings.len(), 5); // "other" and "time_stamp" are ignored
    }

    #[test]
    fn history_for_other_or_missing_device_is_empty() {
        let data = json!({ "999_11_0_0": [] });
        assert!(parse_history(&data, KEY, ADELAIDE, 0).is_empty());
        assert!(parse_history(&Value::Null, KEY, ADELAIDE, 0).is_empty());
    }

    #[test]
    fn history_skips_bad_and_nonexistent_times() {
        let data = json!({ KEY: [
            { "p83022": "1", "time_stamp": "20261004023000" }, // skipped hour
            { "p83022": "2", "time_stamp": "garbage" },
            { "p83022": "3" },
            { "p83022": "4", "time_stamp": "20261004030000" },
        ]});
        let readings = parse_history(&data, KEY, ADELAIDE, 0);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, Some(4.0));
    }

    #[test]
    fn local_time_round_trips() {
        for text in ["20261020110000", "20230221000000", "20260101235500"] {
            let ts = parse_local(text, ADELAIDE).unwrap();
            assert_eq!(format_local(ts, ADELAIDE), text);
        }
        assert_eq!(parse_local("not a time", ADELAIDE), None);
    }

    #[test]
    fn autumn_fall_back_takes_earliest() {
        // 2026-04-05: at 03:00 ACDT (+10:30) clocks go back to 02:00 ACST
        // (+9:30), so local times 02:00-02:59 happen twice.
        let ts = parse_local("20260405023000", ADELAIDE).unwrap();
        let first = 1_775_318_400; // 2026-04-04 16:00:00 UTC = 02:30 ACDT
        assert_eq!(ts, first);
        // 03:00 ACST (after the repeated hour) is 90 real minutes later.
        let after = parse_local("20260405030000", ADELAIDE).unwrap();
        assert_eq!(after - first, 90 * 60);
        // Formatting the later copy of 02:30 (ACST) shows the same text.
        assert_eq!(format_local(first + 3600, ADELAIDE), "20260405023000");
    }

    #[test]
    fn window_is_split_at_fall_back() {
        // Change at 2026-04-05 03:00 ACDT = 2026-04-04 16:30 UTC.
        let change = 1_775_320_200;
        let start = parse_local("20260405010000", ADELAIDE).unwrap();
        let end = start + 6 * 3600;
        let mut from = start;
        let mut spans = Vec::new();
        while from < end {
            let to = next_window_end(from, end, START_WINDOW_SECS, ADELAIDE);
            assert!(to > from);
            spans.push((from, to));
            from = to;
        }
        assert!(spans.iter().any(|&(f, _)| f == change));
        for &(f, t) in &spans {
            assert!(f >= change || t <= change, "window crosses the change");
            let (a, b) = (
                format_local(f, ADELAIDE),
                format_local(t - STEP_SECS, ADELAIDE),
            );
            assert!(a <= b, "{a} > {b}");
        }
        assert_eq!(
            format_local(spans[0].1 - STEP_SECS, ADELAIDE),
            "20260405025500"
        );
        assert_eq!(format_local(spans[1].0, ADELAIDE), "20260405020000");
    }

    #[test]
    fn repeated_hour_is_kept_in_both_windows() {
        let change = 1_775_320_200;
        let pre = parse_local_near("20260405023000", ADELAIDE, change - 3600).unwrap();
        let post = parse_local_near("20260405023000", ADELAIDE, change).unwrap();
        assert_eq!(post - pre, 3600);
        assert_eq!(pre, 1_775_318_400); // 02:30 ACDT
        // parse_history applies the window's offset to every row.
        let data = json!({ KEY: [{ "p1": "1", "time_stamp": "20260405023000" }] });
        assert_eq!(parse_history(&data, KEY, ADELAIDE, change)[0].ts, post);
        assert_eq!(
            parse_history(&data, KEY, ADELAIDE, change - 3600)[0].ts,
            pre
        );
    }

    #[test]
    fn windows_split_at_spring_forward() {
        // Change at 2026-10-04 02:00 ACST = 2026-10-03 16:30 UTC.
        let change = 1_791_045_000;
        let start = change - 3600;
        let to = next_window_end(start, start + 3 * 3600, START_WINDOW_SECS, ADELAIDE);
        assert_eq!(to, change);
        assert_eq!(format_local(to - STEP_SECS, ADELAIDE), "20261004015500");
        let to2 = next_window_end(change, change + 3 * 3600, START_WINDOW_SECS, ADELAIDE);
        assert_eq!(to2, change + 3 * 3600);
        assert_eq!(format_local(change, ADELAIDE), "20261004030000");
    }

    #[test]
    fn window_end_respects_range_end() {
        let start = parse_local("20261020110000", ADELAIDE).unwrap();
        assert_eq!(
            next_window_end(start, start + 600, 3 * 3600, ADELAIDE),
            start + 600
        );
    }

    #[test]
    fn spring_forward_time_does_not_exist() {
        // 2026-10-04: at 02:00 ACST clocks jump to 03:00 ACDT.
        assert_eq!(parse_local("20261004020000", ADELAIDE), None);
        assert_eq!(parse_local("20261004025500", ADELAIDE), None);
        let before = parse_local("20261004015500", ADELAIDE).unwrap();
        let after = parse_local("20261004030000", ADELAIDE).unwrap();
        assert_eq!(after - before, 300); // only 5 real minutes apart
    }

    #[test]
    fn parses_point_info() {
        let data = json!({
            "pageList": [
                { "point_id": 83022, "point_name": "Plant daily yield",
                  "storage_unit": "Wh", "show_unit": "kWh" },
                { "point_id": 83033 },
                { "point_name": "no id" },
            ],
            "rowCount": 74
        });
        let points = parse_points(&data);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].code.as_deref(), Some("p83022"));
        assert_eq!(points[0].name.as_deref(), Some("Plant daily yield"));
        assert_eq!(points[0].unit.as_deref(), Some("Wh"));
        assert_eq!(points[1].name, None);
        assert!(parse_points(&json!({})).is_empty());
    }

    #[test]
    fn budget_decisions() {
        assert_eq!(check_budget(0, 0), Budget::Go);
        assert_eq!(check_budget(1799, 10), Budget::Go);
        assert_eq!(check_budget(1800, 10), Budget::WaitForHour);
        assert_eq!(check_budget(0, 80_000), Budget::MonthlyLimit);
        assert_eq!(check_budget(1800, 80_000), Budget::MonthlyLimit);
    }

    #[test]
    fn next_tick_is_a_minute_after_the_boundary() {
        let noon = 1_792_000_800; // an exact hour (divisible by 3600)
        assert_eq!(
            next_tick_delay(noon + 3 * 60 + 30),
            Duration::from_secs(150)
        );
        assert_eq!(next_tick_delay(noon + 5 * 60), Duration::from_secs(60));
        assert_eq!(next_tick_delay(noon + 5 * 60 + 1), Duration::from_secs(359));
    }

    #[test]
    fn next_tick_delay_stays_in_range() {
        for offset in 0..3600 {
            let secs = next_tick_delay(1_792_000_800 + offset).as_secs();
            assert!((60..=360).contains(&secs), "offset {offset} gave {secs}");
        }
    }

    #[test]
    fn midnight_is_local_not_utc() {
        // 2026-10-20 23:30 local (+10:30) is 13:00 UTC the same day.
        let now = parse_local("20261020233000", ADELAIDE).unwrap();
        let midnight = local_midnight(now, ADELAIDE).unwrap();
        assert_eq!(format_local(midnight, ADELAIDE), "20261020000000");
        // Just after midnight local is still the new day.
        let now = parse_local("20261021000500", ADELAIDE).unwrap();
        assert_eq!(
            local_midnight(now, ADELAIDE),
            parse_local("20261021000000", ADELAIDE)
        );
    }

    #[test]
    fn live_start_uses_its_own_cursor_and_never_chases_a_backfill() {
        let now = 1_800_000_000;
        let midnight = now - 10 * 3600;
        let overlap = LIVE_OVERLAP_SECS;
        // Its own cursor always wins.
        assert_eq!(
            pick_live_start(Some(now - 600), Some(1), midnight, now),
            now - 600 - overlap
        );
        // A fresh database starts at midnight.
        assert_eq!(pick_live_start(None, None, midnight, now), midnight);
        // A backfill in progress (cursor years back) is left alone.
        assert_eq!(
            pick_live_start(None, Some(now - 400 * 86400), midnight, now),
            midnight
        );
        // An older database's recent shared cursor is carried on from.
        assert_eq!(
            pick_live_start(None, Some(now - 600), midnight, now),
            now - 600 - overlap
        );
        let yesterday = now - 30 * 3600;
        assert_eq!(
            pick_live_start(None, Some(yesterday), midnight, now),
            yesterday - overlap
        );
    }

    #[test]
    fn backfill_starts_from_the_cursor_but_not_before_history_from() {
        assert_eq!(backfill_start(None, None), None);
        assert_eq!(backfill_start(None, Some(100)), Some(100));
        assert_eq!(backfill_start(Some(500), None), Some(500));
        assert_eq!(backfill_start(Some(500), Some(100)), Some(500));
        assert_eq!(backfill_start(Some(50), Some(100)), Some(100));
    }

    #[test]
    fn rounds_down_to_five_minutes() {
        assert_eq!(round_down_to_step(1_000), 900);
        assert_eq!(round_down_to_step(900), 900);
    }
}
