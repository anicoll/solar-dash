//! The JSON API: handlers, input checks and the derivation of power from
//! the daily energy counters.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{Days, LocalResult, NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::error;

use super::AppState;
use crate::db::{self, LatestReading};

const MAX_SERIES_POINTS: usize = 20;
const MAX_SERIES_SECS: i64 = 31 * 24 * 3600;
/// `/api/live` ignores a sample older than this: it would show stale data as current.
const LIVE_MAX_AGE_SECS: i64 = 5 * 60;
const MAX_LIVE_SERIES_SECS: i64 = 2 * 24 * 3600;
const MAX_DAILY_DAYS: u64 = 400;
/// Without `from`, `/api/daily` returns this many days.
const DEFAULT_DAILY_DAYS: u64 = 30;
/// The longest time one counter step is spread over (see `derive_power`).
const MAX_SPREAD_SECS: i64 = 30 * 60;

/// Power points shown by `/api/day`: (key, point id).
const POWER_POINTS: [(&str, i64); 3] = [("pv_w", 83033), ("load_w", 83106), ("soc", 83252)];

/// Daily counters (Wh) that `/api/day` turns into power:
/// (key, counter point id, made-up id for the derived series, name).
const DERIVED_POINTS: [(&str, i64, i64, &str); 4] = [
    ("grid_import_w", 83102, -1, "Grid import power"),
    ("grid_export_w", 83072, -2, "Grid export power"),
    ("battery_charge_w", 83322, -3, "Battery charge power"),
    ("battery_discharge_w", 83323, -4, "Battery discharge power"),
];

/// An error that becomes a JSON response like `{"error":"..."}`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "unauthorized".to_string(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

// Lets handlers use `?` on database calls. The details go to the log, not to the client.
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        error!("request failed: {err:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".to_string(),
        }
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// A series as sent to the client.
#[derive(Debug, Serialize)]
struct Series {
    /// Only on `/api/day`, so the frontend needs no magic point ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'static str>,
    point_id: i64,
    name: Option<String>,
    unit: Option<String>,
    /// `[ts, value]` pairs, oldest first.
    data: Vec<(i64, f64)>,
}

/// Loads one stored point as a series.
async fn stored_series(
    state: &AppState,
    key: Option<&'static str>,
    point_id: i64,
    from: i64,
    to: i64,
) -> Result<Series, ApiError> {
    let (name, unit) = db::point_info(&state.pool, point_id)
        .await?
        .unwrap_or((None, None));
    let data = db::readings_in_range(&state.pool, state.plant_id, point_id, from, to).await?;
    Ok(Series {
        key,
        point_id,
        name,
        unit,
        data,
    })
}

pub async fn healthz(State(state): State<AppState>) -> ApiResult {
    let last = db::max_reading_ts(&state.pool, state.plant_id).await?;
    Ok(Json(json!({ "ok": true, "last_reading_ts": last })))
}

pub async fn latest(State(state): State<AppState>) -> ApiResult {
    let rows: Vec<LatestReading> = db::latest_readings(&state.pool, state.plant_id).await?;
    let points: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "point_id": r.point_id,
                "name": r.name,
                "unit": r.unit,
                "ts": r.ts,
                "value": r.value,
            })
        })
        .collect();
    Ok(Json(json!({ "points": points })))
}

/// The newest Modbus sample, or `{"sample": null}` if there is none or it
/// is too old to call "live" (for example when Modbus is off).
pub async fn live(State(state): State<AppState>) -> ApiResult {
    let sample = db::latest_live_sample(&state.pool).await?;
    let fresh = sample.filter(|s| s.ts >= db::now() - LIVE_MAX_AGE_SECS);
    Ok(Json(json!({ "sample": fresh })))
}

#[derive(Deserialize)]
pub struct LiveSeriesParams {
    from: Option<String>,
    to: Option<String>,
}

/// Live samples averaged per minute, as `{"data":[[ts,pv_w,load_w,grid_w,battery_w,soc_pct],...]}`.
pub async fn live_series(
    State(state): State<AppState>,
    Query(params): Query<LiveSeriesParams>,
) -> ApiResult {
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_LIVE_SERIES_SECS {
        return Err(ApiError::bad_request("the range can be at most 2 days"));
    }

    // `to` is inclusive here; the query's end is exclusive.
    let samples = db::live_samples_in_range(&state.pool, from, to + 1).await?;
    let data: Vec<Value> = samples
        .iter()
        .map(|s| json!([s.ts, s.pv_w, s.load_w, s.grid_w, s.battery_w, s.soc_pct]))
        .collect();
    Ok(Json(json!({ "data": data })))
}

// Every parameter is an `Option<String>` that we parse ourselves. If we asked
// axum to parse numbers, a typo would give a plain-text error instead of our
// JSON one.
#[derive(Deserialize)]
pub struct SeriesParams {
    points: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

/// Parses `83033,83106` into point ids.
fn parse_points(text: &str) -> Result<Vec<i64>, ApiError> {
    let points: Vec<i64> = text
        .split(',')
        .map(|part| part.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| ApiError::bad_request("points must be comma-separated point ids"))?;
    if points.is_empty() || points.len() > MAX_SERIES_POINTS {
        return Err(ApiError::bad_request(format!(
            "points must have between 1 and {MAX_SERIES_POINTS} ids"
        )));
    }
    Ok(points)
}

fn parse_unix(name: &str, text: Option<&str>) -> Result<i64, ApiError> {
    text.and_then(|t| t.parse().ok())
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a unix time in seconds")))
}

pub async fn series(
    State(state): State<AppState>,
    Query(params): Query<SeriesParams>,
) -> ApiResult {
    let points = parse_points(params.points.as_deref().unwrap_or(""))?;
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_SERIES_SECS {
        return Err(ApiError::bad_request("the range can be at most 31 days"));
    }

    let mut series = Vec::new();
    for point_id in points {
        // `to` is inclusive here; the query's end is exclusive.
        series.push(stored_series(&state, None, point_id, from, to + 1).await?);
    }
    Ok(Json(json!({ "series": series })))
}

fn parse_date(name: &str, text: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| ApiError::bad_request(format!("{name} must look like 2026-10-20")))
}

/// Unix time of the first moment of `date` in `tz`.
///
/// Midnight normally exists exactly once. In a zone whose clocks jump over
/// midnight we take the first hour that does exist; if clocks go back over
/// midnight we take the earlier of the two.
fn local_midnight(date: NaiveDate, tz: Tz) -> Option<i64> {
    for hour in 0..3 {
        let naive = date.and_hms_opt(hour, 0, 0)?;
        match tz.from_local_datetime(&naive) {
            LocalResult::Single(t) => return Some(t.timestamp()),
            LocalResult::Ambiguous(earliest, _) => return Some(earliest.timestamp()),
            LocalResult::None => continue,
        }
    }
    None
}

/// The local day `date` as `[start, end)` in unix seconds. On a daylight
/// saving change this is 23 or 25 hours long, not 24.
fn day_bounds(date: NaiveDate, tz: Tz) -> Option<(i64, i64)> {
    let next = date.checked_add_days(Days::new(1))?;
    Some((local_midnight(date, tz)?, local_midnight(next, tz)?))
}

fn day_bounds_or_error(date: NaiveDate, tz: Tz) -> Result<(i64, i64), ApiError> {
    day_bounds(date, tz).ok_or_else(|| ApiError::bad_request("date is out of range"))
}

fn today(tz: Tz) -> NaiveDate {
    chrono::Utc::now().with_timezone(&tz).date_naive()
}

/// Turns a daily energy counter (Wh, `(ts, value)` pairs) into average power
/// (W) at each row after the first, oldest first.
///
/// iSolarCloud counters only move in 100 Wh steps, so a counter can sit still
/// for half an hour and then jump. Turning each 5-minute change straight into
/// power would show 0 W and then a 1.2 kW spike. Instead each step is spread
/// evenly over the time since the counter last moved, at most
/// [`MAX_SPREAD_SECS`]: `W = ΔWh * 3600 / seconds`.
///
/// - A negative change is the counter resetting at midnight: that row is 0
///   and counting starts again from the new value.
/// - Rows after the last step stay at 0 until the counter moves again.
fn derive_power(counter: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let Some(&(first_ts, first_value)) = counter.first() else {
        return Vec::new();
    };
    let mut power: Vec<(i64, f64)> = Vec::new();
    // When the counter last moved, and its value then.
    let (mut last_ts, mut last_value) = (first_ts, first_value);

    for &(ts, value) in &counter[1..] {
        power.push((ts, 0.0));
        if value < last_value {
            (last_ts, last_value) = (ts, value);
        } else if value > last_value {
            let spread_from = last_ts.max(ts - MAX_SPREAD_SECS);
            let watts = (value - last_value) * 3600.0 / (ts - spread_from) as f64;
            // This row and the earlier rows the step covers share the energy.
            for row in power.iter_mut().rev().take_while(|row| row.0 > spread_from) {
                row.1 = watts;
            }
            (last_ts, last_value) = (ts, value);
        }
    }
    power
}

#[derive(Deserialize)]
pub struct DayParams {
    date: Option<String>,
}

pub async fn day(State(state): State<AppState>, Query(params): Query<DayParams>) -> ApiResult {
    let date = match params.date.as_deref() {
        Some(text) => parse_date("date", text)?,
        None => today(state.tz),
    };
    let (start, end) = day_bounds_or_error(date, state.tz)?;

    let mut series = Vec::new();
    for (key, point_id) in POWER_POINTS {
        series.push(stored_series(&state, Some(key), point_id, start, end).await?);
    }
    for (key, counter_id, derived_id, name) in DERIVED_POINTS {
        let counter =
            db::readings_in_range(&state.pool, state.plant_id, counter_id, start, end).await?;
        series.push(Series {
            key: Some(key),
            point_id: derived_id,
            name: Some(name.to_string()),
            unit: Some("W".to_string()),
            data: derive_power(&counter),
        });
    }
    Ok(Json(json!({
        "date": date.to_string(),
        "start": start,
        "end": end,
        "series": series,
    })))
}

#[derive(Deserialize)]
pub struct DailyParams {
    from: Option<String>,
    to: Option<String>,
}

/// Energy totals of one local day, in kWh. `None` means no data that day.
#[derive(Debug, Serialize)]
struct DayTotals {
    date: String,
    pv_kwh: Option<f64>,
    load_kwh: Option<f64>,
    import_kwh: Option<f64>,
    export_kwh: Option<f64>,
    battery_charge_kwh: Option<f64>,
    battery_discharge_kwh: Option<f64>,
    self_consumed_kwh: Option<f64>,
}

/// The day's total for one counter: its largest value within the local day,
/// because the counter only grows until it resets at midnight. Wh to kWh.
async fn day_max_kwh(
    state: &AppState,
    point_id: i64,
    start: i64,
    end: i64,
) -> Result<Option<f64>, ApiError> {
    let max = db::max_value_in_range(&state.pool, state.plant_id, point_id, start, end).await?;
    Ok(max.map(|wh| wh / 1000.0))
}

async fn day_totals(state: &AppState, date: NaiveDate) -> Result<DayTotals, ApiError> {
    // Each day gets its own [start, end), so a 23 or 25 hour day is handled correctly.
    let (start, end) = day_bounds_or_error(date, state.tz)?;
    Ok(DayTotals {
        date: date.to_string(),
        pv_kwh: day_max_kwh(state, 83022, start, end).await?,
        load_kwh: day_max_kwh(state, 83118, start, end).await?,
        import_kwh: day_max_kwh(state, 83102, start, end).await?,
        export_kwh: day_max_kwh(state, 83072, start, end).await?,
        battery_charge_kwh: day_max_kwh(state, 83322, start, end).await?,
        battery_discharge_kwh: day_max_kwh(state, 83323, start, end).await?,
        self_consumed_kwh: day_max_kwh(state, 83097, start, end).await?,
    })
}

pub async fn daily(State(state): State<AppState>, Query(params): Query<DailyParams>) -> ApiResult {
    let to = match params.to.as_deref() {
        Some(text) => parse_date("to", text)?,
        None => today(state.tz),
    };
    let from = match params.from.as_deref() {
        Some(text) => parse_date("from", text)?,
        None => to - Days::new(DEFAULT_DAILY_DAYS - 1),
    };
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    let count = (to - from).num_days() as u64 + 1;
    if count > MAX_DAILY_DAYS {
        return Err(ApiError::bad_request(format!(
            "the range can be at most {MAX_DAILY_DAYS} days"
        )));
    }

    let mut days = Vec::new();
    for date in from.iter_days().take(count as usize) {
        days.push(day_totals(&state, date).await?);
    }
    Ok(Json(json!({ "days": days })))
}

#[cfg(test)]
mod tests {
    use axum_extra::extract::cookie::Key;

    use super::*;
    use crate::db::{Point, Reading};

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    #[test]
    fn derives_power_from_counter_deltas() {
        // +10 Wh in 5 minutes is 120 W. +15 Wh over the next 10 minutes is 90 W.
        let counter = [(0, 100.0), (300, 110.0), (600, 110.0), (900, 125.0)];
        assert_eq!(
            derive_power(&counter),
            vec![(300, 120.0), (600, 90.0), (900, 90.0)]
        );
    }

    #[test]
    fn coarse_steps_are_spread_out() {
        // A 100 Wh step after 15 still minutes is 400 W over those 15 minutes,
        // not a 1.2 kW spike.
        let counter = [(0, 1100.0), (300, 1100.0), (600, 1100.0), (900, 1200.0)];
        assert_eq!(
            derive_power(&counter),
            vec![(300, 400.0), (600, 400.0), (900, 400.0)]
        );
        // After a long still spell, the step is spread over the last 30
        // minutes only (100 Wh in 30 minutes is 200 W).
        let counter: Vec<_> = (0..=12)
            .map(|i| (i * 300, if i == 12 { 1200.0 } else { 1100.0 }))
            .collect();
        let power = derive_power(&counter);
        assert!(power[..6].iter().all(|row| row.1 == 0.0));
        assert!(power[6..].iter().all(|row| row.1 == 200.0));
    }

    #[test]
    fn midnight_reset_counts_as_zero() {
        let counter = [(0, 9000.0), (300, 9010.0), (600, 0.0), (900, 5.0)];
        assert_eq!(
            derive_power(&counter),
            vec![(300, 120.0), (600, 0.0), (900, 60.0)]
        );
    }

    #[test]
    fn missing_rows_use_the_real_gap() {
        // The row at 600 is missing, so +45 Wh covers 10 minutes: 270 W.
        let counter = [(0, 0.0), (300, 5.0), (900, 50.0), (1200, 51.0)];
        assert_eq!(
            derive_power(&counter),
            vec![(300, 60.0), (900, 270.0), (1200, 12.0)]
        );
        assert!(derive_power(&[]).is_empty());
        assert!(derive_power(&[(0, 1.0)]).is_empty());
    }

    #[test]
    fn dst_days_are_not_24_hours() {
        // Clocks go back on 2026-04-05 and forward on 2026-10-04.
        let hours = |date: &str| {
            let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
            let (start, end) = day_bounds(date, ADELAIDE).unwrap();
            (end - start) / 3600
        };
        assert_eq!(hours("2026-04-05"), 25);
        assert_eq!(hours("2026-10-04"), 23);
        assert_eq!(hours("2026-10-20"), 24);
    }

    #[tokio::test]
    async fn daily_max_respects_the_local_day_across_dst() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let point = Point {
            point_id: 83022,
            code: None,
            name: None,
            unit: Some("Wh".to_string()),
        };
        db::upsert_point(&pool, &point).await.unwrap();

        // Adelaide clocks go back on Sunday 2026-04-05, so that day has 25
        // hours: it starts at 13:30 UTC on the 4th and ends at 14:30 UTC on the 5th.
        let utc = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
        let rows = [
            // Saturday evening, 23:55 local: the last reading of the 4th.
            (utc("2026-04-04T13:25:00Z"), 100_000.0),
            // Sunday 00:00 local: the counter has reset.
            (utc("2026-04-04T13:30:00Z"), 0.0),
            (utc("2026-04-05T10:00:00Z"), 8_000.0),
            // Sunday 23:55 local (ACST). A 24-hour day would have ended before this.
            (utc("2026-04-05T14:25:00Z"), 9_000.0),
            // Monday 00:00 local.
            (utc("2026-04-05T14:30:00Z"), 0.0),
        ];
        let readings: Vec<Reading> = rows
            .iter()
            .map(|&(ts, value)| Reading {
                point_id: 83022,
                ts,
                value: Some(value),
            })
            .collect();
        db::upsert_readings(&pool, 7, &readings).await.unwrap();

        let key = Key::generate();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, key).unwrap();

        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let saturday = day_totals(&state, date("2026-04-04")).await.unwrap();
        let sunday = day_totals(&state, date("2026-04-05")).await.unwrap();
        let monday = day_totals(&state, date("2026-04-06")).await.unwrap();
        assert_eq!(saturday.pv_kwh, Some(100.0));
        assert_eq!(sunday.pv_kwh, Some(9.0));
        assert_eq!(monday.pv_kwh, Some(0.0));
        // No data for the other counters.
        assert_eq!(sunday.load_kwh, None);
    }
}
