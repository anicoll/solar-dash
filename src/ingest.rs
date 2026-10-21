//! Downloading history from iSolarCloud into SQLite.
//!
//! The parsing and time helpers are pure functions (no network, no database)
//! so they can be unit tested. [`sync_range`] ties them together, and is used
//! by both [`backfill`] and the live loop, [`run_live_sync`].

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
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

/// The history has one row every 5 minutes.
const STEP_SECS: i64 = 5 * 60;
/// First window size to try. It is halved if the API rejects it.
/// The real API rejected 24, 12 and 6 hours but accepted 3, so start there.
const START_WINDOW_SECS: i64 = 3 * 3600;
/// One hour is confirmed to work, so never go below it.
const MIN_WINDOW_SECS: i64 = 3600;

// The API allows 2,000 calls per hour and 100,000 per month; stay below both.
const MAX_CALLS_PER_HOUR: i64 = 1500;
const MAX_CALLS_PER_30_DAYS: i64 = 80_000;
const BUDGET_RECHECK: Duration = Duration::from_secs(60);
/// Pause between calls, to be polite to the server.
const CALL_PAUSE: Duration = Duration::from_secs(1);

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

/// Splits `start..end` into consecutive windows of at most `step` seconds.
/// Each window ends where the next begins, so there are no gaps or overlaps.
pub fn windows(start: i64, end: i64, step: i64) -> Vec<(i64, i64)> {
    let mut result = Vec::new();
    let mut from = start;
    while from < end {
        let to = (from + step).min(end);
        result.push((from, to));
        from = to;
    }
    result
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
pub fn parse_history(result_data: &Value, ps_key: &str, tz: Tz) -> Vec<Reading> {
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
            .and_then(|s| parse_local(s, tz))
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
        Ok(Self {
            plant_id,
            tz,
            ps_key: format!("{plant_id}_11_0_0"),
            points,
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
        let (_, window_end) = windows(start, end_ts, window)[0];
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

        db::save_raw_response(
            pool,
            HISTORY_ENDPOINT,
            Some(start),
            Some(window_end),
            &data.to_string(),
        )
        .await?;
        let readings = parse_history(&data, &plant.ps_key, tz);
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

/// A sync that logs in again and retries once if iSolarCloud reports an error.
/// The token-expiry codes are not confirmed, so any `ApiError` gets one retry.
async fn sync_with_relogin(
    client: &mut Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
) -> Result<()> {
    match sync_once(client, pool, config).await {
        Err(err) if err.downcast_ref::<ApiError>().is_some() => {
            warn!("iSolarCloud returned an error ({err:#}); logging in again");
            client.login().await?;
            sync_once(client, pool, config).await
        }
        other => other,
    }
}

/// Keeps the database up to date until `shutdown` completes.
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
    loop {
        // Dropping an unfinished sync is safe: each window is saved
        // atomically, along with its cursor.
        tokio::select! {
            _ = &mut shutdown => break,
            result = sync_with_relogin(&mut client, pool, config) => {
                // A failed tick is not fatal: try again on the next one.
                if let Err(err) = result {
                    warn!("sync failed: {err:#}");
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
        let readings = parse_history(&data, KEY, ADELAIDE);

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
        assert!(parse_history(&data, KEY, ADELAIDE).is_empty());
        assert!(parse_history(&Value::Null, KEY, ADELAIDE).is_empty());
    }

    #[test]
    fn history_skips_bad_and_nonexistent_times() {
        let data = json!({ KEY: [
            { "p83022": "1", "time_stamp": "20261004023000" }, // skipped hour
            { "p83022": "2", "time_stamp": "garbage" },
            { "p83022": "3" },
            { "p83022": "4", "time_stamp": "20261004030000" },
        ]});
        let readings = parse_history(&data, KEY, ADELAIDE);
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
    fn spring_forward_time_does_not_exist() {
        // 2026-10-04: at 02:00 ACST clocks jump to 03:00 ACDT.
        assert_eq!(parse_local("20261004020000", ADELAIDE), None);
        assert_eq!(parse_local("20261004025500", ADELAIDE), None);
        let before = parse_local("20261004015500", ADELAIDE).unwrap();
        let after = parse_local("20261004030000", ADELAIDE).unwrap();
        assert_eq!(after - before, 300); // only 5 real minutes apart
    }

    #[test]
    fn windows_cover_range_exactly() {
        let w = windows(0, 250, 100);
        assert_eq!(w, vec![(0, 100), (100, 200), (200, 250)]);
        assert_eq!(windows(0, 200, 100), vec![(0, 100), (100, 200)]);
        assert_eq!(windows(5, 6, 100), vec![(5, 6)]);
        assert!(windows(10, 10, 100).is_empty());
        assert!(windows(10, 5, 100).is_empty());
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
        assert_eq!(check_budget(1499, 10), Budget::Go);
        assert_eq!(check_budget(1500, 10), Budget::WaitForHour);
        assert_eq!(check_budget(0, 80_000), Budget::MonthlyLimit);
        assert_eq!(check_budget(1500, 80_000), Budget::MonthlyLimit);
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
    fn rounds_down_to_five_minutes() {
        assert_eq!(round_down_to_step(1_000), 900);
        assert_eq!(round_down_to_step(900), 900);
    }
}
