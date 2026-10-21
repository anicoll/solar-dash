//! Downloading history from iSolarCloud into SQLite.
//!
//! The parsing and time helpers are pure functions (no network, no database)
//! so they can be unit tested. [`backfill`] ties them together.

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
/// Name of this job in the `sync_state` table.
const JOB: &str = "backfill";

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
    for &point_id in wanted {
        let point = Point {
            point_id,
            code: Some(format!("p{point_id}")),
            name: None,
            unit: None,
        };
        db::upsert_point(pool, &point).await?;
    }

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

/// Downloads 5-minute history from local midnight of `from` until now.
/// Safe to stop and run again: it resumes from the saved cursor.
pub async fn backfill(
    client: &Client,
    pool: &SqlitePool,
    config: &IsolarCloud,
    from: NaiveDate,
) -> Result<()> {
    let plant_id = config.plant_id.ok_or_else(|| {
        anyhow!("backfill needs plant_id in config.toml (run login-test to see your plants)")
    })?;
    let tz: Tz = config
        .timezone
        .parse()
        .map_err(|e| anyhow!("invalid timezone {:?}: {e}", config.timezone))?;
    let ps_key = format!("{plant_id}_11_0_0");
    let points = config
        .points
        .iter()
        .map(|id| format!("p{id}"))
        .collect::<Vec<_>>()
        .join(",");

    let from_ts = parse_local(&from.format("%Y%m%d000000").to_string(), tz)
        .ok_or_else(|| anyhow!("{from} has no local midnight in {tz}"))?;
    let cursor = db::get_cursor(pool, JOB).await?;
    let mut start = cursor.map_or(from_ts, |c| c.max(from_ts));
    let now = db::now();
    let end = now - now % STEP_SECS;
    if start >= end {
        info!("history is already up to date");
        return Ok(());
    }
    match cursor {
        Some(c) if c >= from_ts => info!(resume_at = %format_local(c, tz), "resuming backfill"),
        _ => info!(start = %format_local(start, tz), "starting backfill"),
    }

    sync_points(client, pool, &config.points, &ps_key).await?;

    let mut window = START_WINDOW_SECS;
    while start < end {
        // `window_end` is exclusive. The API includes its end time, so we ask
        // for one step less; the next window then starts right after.
        let (_, window_end) = windows(start, end, window)[0];
        let params = json!({
            "ps_key_list": [&ps_key],
            "points": points,
            "start_time_stamp": format_local(start, tz),
            "end_time_stamp": format_local(window_end - STEP_SECS, tz),
            "minute_interval": "5",
        });
        debug!(window_hours = window / 3600, "requesting window");

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
        let readings = parse_history(&data, &ps_key, tz);
        db::upsert_readings(pool, plant_id, &readings).await?;
        db::set_cursor(pool, JOB, window_end).await?;

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
    info!("backfill finished");
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
}
