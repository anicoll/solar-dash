//! The JSON API: handlers, input checks and the derivation of power from
//! the daily energy counters.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Days, LocalResult, NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::error;

use super::AppState;
use super::costs::round2;
use crate::db::{self, LatestReading};
use crate::ev_cost;
use crate::modbus::Sample;

const MAX_SERIES_POINTS: usize = 20;
const MAX_SERIES_SECS: i64 = 31 * 24 * 3600;
/// `/api/live` ignores a sample older than this: it would show stale data as current.
const LIVE_MAX_AGE_SECS: i64 = 5 * 60;
const MAX_LIVE_SERIES_SECS: i64 = 2 * 24 * 3600;
const MAX_DAILY_DAYS: u64 = 400;
const MAX_EV_SESSIONS_SECS: i64 = MAX_DAILY_DAYS as i64 * 24 * 3600;
/// Without `from`, `/api/daily` returns this many days.
pub(super) const DEFAULT_DAILY_DAYS: u64 = 30;
/// Without `from`, `/api/ev/summary` covers this many days (about 12 months).
const DEFAULT_SUMMARY_DAYS: u64 = 365;
/// The longest time one counter step is spread over (see `derive_power`).
pub(super) const MAX_SPREAD_SECS: i64 = 30 * 60;
/// The longest time one live sample is held (see `integrate`). Samples come
/// every ~15 s, so a longer step means the dongle dropped out.
pub(super) const MAX_HOLD_SECS: i64 = 120;
/// Today, or a day with no iSolarCloud counters, needs this share covered by
/// live samples before we use them at all. For today it is the share of the
/// time since midnight. It stays at 0.9 there because a stricter bar would
/// flip today between live and cloud numbers as the dongle drops out, and the
/// cloud counters lag behind anyway.
const MIN_LIVE_COVERAGE: f64 = 0.9;
/// A finished day needs this share covered by live samples to be preferred
/// over the iSolarCloud counters. Below it (but at or above
/// [`MIN_LIVE_COVERAGE`]) the live numbers are only a fallback for a day with
/// no cloud data, because up to 10% of the day's energy could be missing.
const MIN_FINISHED_LIVE_COVERAGE: f64 = 0.98;

/// Battery state of charge. iSolarCloud stores it as a fraction (0.674),
/// but its unit is "%", so the API scales it to a percentage (67.4).
pub(super) const SOC_POINT: i64 = 83252;

/// Power points shown by `/api/day`: (key, point id).
const POWER_POINTS: [(&str, i64); 3] = [("pv_w", 83033), ("load_w", 83106), ("soc", SOC_POINT)];

/// A stored value as the API shows it (see [`SOC_POINT`]).
pub(super) fn display_value(point_id: i64, value: f64) -> f64 {
    if point_id == SOC_POINT {
        value * 100.0
    } else {
        value
    }
}

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

    pub(super) fn bad_request(message: impl Into<String>) -> Self {
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

pub(super) type ApiResult = Result<Json<Value>, ApiError>;

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
    let data = db::readings_in_range(&state.pool, state.plant_id, point_id, from, to)
        .await?
        .into_iter()
        .map(|(ts, value)| (ts, display_value(point_id, value)))
        .collect();
    Ok(Series {
        key,
        point_id,
        name,
        unit,
        data,
    })
}

/// Public liveness check. It deliberately reveals nothing about the data.
pub async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
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
                "value": display_value(r.point_id, r.value),
            })
        })
        .collect();
    Ok(Json(json!({ "points": points })))
}

/// `{"sample": ..., "car": ...}`. The sample is the newest Modbus one, or
/// null if there is none or it is too old to call "live" (for example when
/// Modbus is off). It comes from memory; the database is only read until the
/// poller's first sample after a restart.
///
/// The car is TeslaMate's last known state, or null if TeslaMate is off or
/// has sent nothing yet. It gets no age limit: TeslaMate's values are
/// retained and stay true while the car sleeps. `car.updated_ts` says how
/// old they are.
pub async fn live(State(state): State<AppState>) -> ApiResult {
    let sample = match state.latest.get() {
        Some(sample) => Some(sample),
        None => db::latest_live_sample(&state.pool).await?,
    };
    let fresh = sample.filter(|s| s.ts >= db::now() - LIVE_MAX_AGE_SECS);
    Ok(Json(json!({ "sample": fresh, "car": state.car.get() })))
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

/// A charging session with where its energy came from and what it cost.
/// The extra fields are null when the session cannot be costed.
#[derive(Debug, Serialize)]
struct CostedSession {
    #[serde(flatten)]
    session: db::ChargeSession,
    solar_kwh: Option<f64>,
    battery_kwh: Option<f64>,
    grid_kwh: Option<f64>,
    /// Dollars paid for the grid part.
    cost: Option<f64>,
    /// Dollars the solar part would have earned if exported instead.
    solar_value: Option<f64>,
    /// The share (0 to 1) of the session that live samples cover.
    coverage: Option<f64>,
}

/// What a session at home cost, worked out from the live samples. `None`
/// without a `[tariff]` or for a session with no energy.
async fn home_session_cost(
    state: &AppState,
    session: &db::ChargeSession,
    now: i64,
) -> Result<Option<ev_cost::SessionCost>, ApiError> {
    let Some(schedule) = &state.tariff else {
        return Ok(None);
    };
    // `to` is exclusive in the query, so add a second to include the end.
    let end = session.ended_at.unwrap_or(now);
    let samples = db::live_samples_raw(&state.pool, session.started_at, end + 1).await?;
    Ok(ev_cost::session_cost(
        session, &samples, schedule, state.tz, now,
    ))
}

/// Charging sessions that overlap the range, as
/// `{"sessions":[{id,started_at,ended_at,energy_added_kwh,energy_used_kwh,start_soc_pct,end_soc_pct,max_power_kw,geofence,solar_kwh,battery_kwh,grid_kwh,cost,solar_value,coverage},...]}`.
/// A session that has not ended has `ended_at` null and counts as running
/// until now. The list is empty when TeslaMate is off.
///
/// The last six fields are worked out from the live samples on each request
/// (see `ev_cost`). They are null without a `[tariff]`, for a session away
/// from home (a geofence other than "Home") and for one with no energy.
pub async fn ev_sessions(
    State(state): State<AppState>,
    Query(params): Query<LiveSeriesParams>,
) -> ApiResult {
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_EV_SESSIONS_SECS {
        return Err(ApiError::bad_request("the range can be at most 400 days"));
    }

    let now = db::now();
    let mut costed = Vec::new();
    for session in db::charge_sessions_in_range(&state.pool, from, to, now).await? {
        let cost = if ev_cost::is_home(&session) {
            home_session_cost(&state, &session, now).await?
        } else {
            None
        };
        costed.push(CostedSession {
            solar_kwh: cost.as_ref().map(|c| round2(c.solar_kwh)),
            battery_kwh: cost.as_ref().map(|c| round2(c.battery_kwh)),
            grid_kwh: cost.as_ref().map(|c| round2(c.grid_kwh)),
            cost: cost.as_ref().map(|c| round2(c.cost_cents / 100.0)),
            solar_value: cost.as_ref().map(|c| round2(c.solar_value_cents / 100.0)),
            coverage: cost.as_ref().map(|c| round2(c.coverage)),
            session,
        });
    }
    Ok(Json(json!({ "sessions": costed })))
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

/// Charging, km and petrol for each month of the range, as
/// `{"configured":bool,"months":[{month,km,home_kwh,home_cost,away_kwh,away_cost,charging_cost,petrol_litres,petrol_cost,saved,ev_cents_per_km,petrol_cents_per_km},...],"total":{...},"petrol_cents_per_litre":..,"petrol_litres_per_100km":..}`.
/// The dates are local, `from` and `to` included, at most 400 days; without
/// `from` it covers the last [`DEFAULT_SUMMARY_DAYS`] days. Money is in dollars.
///
/// km come from the daily odometer readings, and each session counts in the
/// month it started. Home sessions are costed like `/api/ev/sessions` does;
/// away sessions at `away_cents_per_kwh` (see `ev_cost::summarise`).
/// `configured` is false, and the money fields are null, without a
/// `petrol_cents_per_litre` in `[teslamate]`.
pub async fn ev_summary(
    State(state): State<AppState>,
    Query(params): Query<DailyParams>,
) -> ApiResult {
    let (from, count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        MAX_DAILY_DAYS,
        DEFAULT_SUMMARY_DAYS,
    )?;
    let to = from + Days::new(count - 1);
    let (start, _) = day_bounds_or_error(from, state.tz)?;
    let (_, end) = day_bounds_or_error(to, state.tz)?;

    let odometer = db::odometer_in_range(&state.pool, &from.to_string(), &to.to_string()).await?;
    let km_by_month = ev_cost::monthly_km(&odometer, &from.to_string());

    let prices = state.ev_prices;
    let now = db::now();
    let mut sessions = Vec::new();
    // The query includes sessions that began before the range and are still
    // going; they belong to an earlier month.
    for session in db::charge_sessions_in_range(&state.pool, start, end - 1, now).await? {
        let (Some(kwh), Some(started)) = (
            ev_cost::session_energy_kwh(&session),
            DateTime::from_timestamp(session.started_at, 0),
        ) else {
            continue;
        };
        if session.started_at < start {
            continue;
        }
        let at_home = ev_cost::is_home(&session);
        let cost_cents = if !at_home {
            prices.away_cents_per_kwh.map(|cents| cents * kwh)
        } else if prices.petrol_cents_per_litre.is_some() {
            home_session_cost(&state, &session, now)
                .await?
                .map(|cost| cost.cost_cents)
        } else {
            // Without a petrol price the money is not shown, so skip the work.
            None
        };
        sessions.push(ev_cost::SessionEnergy {
            month: started.with_timezone(&state.tz).format("%Y-%m").to_string(),
            at_home,
            kwh,
            cost_cents,
        });
    }

    let summary = ev_cost::summarise(&prices, &km_by_month, &sessions);
    Ok(Json(json!({
        "configured": summary.configured,
        "months": summary.months,
        "total": summary.total,
        "petrol_cents_per_litre": prices.petrol_cents_per_litre,
        "petrol_litres_per_100km": prices.petrol_litres_per_100km,
    })))
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

pub(super) fn day_bounds_or_error(date: NaiveDate, tz: Tz) -> Result<(i64, i64), ApiError> {
    day_bounds(date, tz).ok_or_else(|| ApiError::bad_request("date is out of range"))
}

pub(super) fn today(tz: Tz) -> NaiveDate {
    chrono::Utc::now().with_timezone(&tz).date_naive()
}

/// One step of a daily energy counter, spread over the time it covers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Spread {
    /// Unix seconds. The energy is taken to flow evenly from `start` to `end`.
    pub start: i64,
    pub end: i64,
    /// The counter's rise, in Wh.
    pub wh: f64,
}

/// Finds each step of a daily energy counter (`(ts, Wh)` pairs, oldest first)
/// and the time to spread it over.
///
/// iSolarCloud counters only move in 100 Wh steps, so a counter can sit still
/// for half an hour and then jump. A step is spread evenly over the time since
/// the counter last moved, at most [`MAX_SPREAD_SECS`]. The spans never
/// overlap, so the energies add up to the counter's total rise.
///
/// A drop is the counter resetting at midnight: it adds no energy and
/// counting starts again from the new value.
pub(super) fn spread_steps(counter: &[(i64, f64)]) -> Vec<Spread> {
    let Some(&(first_ts, first_value)) = counter.first() else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    // When the counter last moved, and its value then.
    let (mut last_ts, mut last_value) = (first_ts, first_value);

    for &(ts, value) in &counter[1..] {
        if value > last_value {
            steps.push(Spread {
                start: last_ts.max(ts - MAX_SPREAD_SECS),
                end: ts,
                wh: value - last_value,
            });
        }
        if value != last_value {
            (last_ts, last_value) = (ts, value);
        }
    }
    steps
}

/// Turns a daily energy counter (Wh, `(ts, value)` pairs) into average power
/// (W) at each row after the first, oldest first.
///
/// Turning each 5-minute change straight into power would show 0 W and then a
/// 1.2 kW spike, so each step is spread out as [`spread_steps`] describes:
/// `W = ΔWh * 3600 / seconds`.
///
/// - A negative change is the counter resetting at midnight: that row is 0.
/// - Rows after the last step stay at 0 until the counter moves again.
pub(super) fn derive_power(counter: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let mut power: Vec<(i64, f64)> = counter.iter().skip(1).map(|&(ts, _)| (ts, 0.0)).collect();
    for step in spread_steps(counter) {
        let watts = step.wh * 3600.0 / (step.end - step.start) as f64;
        // This row and the earlier rows the step covers share the energy.
        // `partition_point` finds where the rows (sorted by time) pass a time.
        let first = power.partition_point(|row| row.0 <= step.start);
        let last = power.partition_point(|row| row.0 <= step.end);
        for row in &mut power[first..last] {
            row.1 = watts;
        }
    }
    power
}

/// The result of [`integrate`].
#[derive(Debug, PartialEq)]
struct Integral {
    /// Energy while the value was above zero, in kWh.
    positive_kwh: f64,
    /// Energy while the value was below zero, in kWh, as a positive number.
    negative_kwh: f64,
    /// Seconds that were counted. Time in a longer gap is not counted.
    covered_secs: i64,
}

/// Adds up power samples (`(ts, watts)`, oldest first) into energy.
///
/// Each sample's value is held until the next sample (a "left rectangle"),
/// for at most [`MAX_HOLD_SECS`]. After a longer gap the missing time is
/// unknown: it adds no energy and no covered seconds. The last sample has no
/// next sample, so it adds nothing. Positive and negative values are kept
/// apart, so grid power gives import and export separately.
fn integrate(samples: &[(i64, f64)]) -> Integral {
    let mut positive_ws = 0.0; // watt-seconds
    let mut negative_ws = 0.0;
    let mut covered_secs = 0;
    // `windows(2)` gives each pair of neighbouring samples.
    for pair in samples.windows(2) {
        let (ts, watts) = pair[0];
        let secs = (pair[1].0 - ts).min(MAX_HOLD_SECS);
        if watts >= 0.0 {
            positive_ws += watts * secs as f64;
        } else {
            negative_ws -= watts * secs as f64;
        }
        covered_secs += secs;
    }
    // 1 kWh is 3,600,000 watt-seconds.
    Integral {
        positive_kwh: positive_ws / 3.6e6,
        negative_kwh: negative_ws / 3.6e6,
        covered_secs,
    }
}

/// How far the live samples can be trusted for one day.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LiveCoverage {
    /// Too little: use the cloud counters.
    Thin,
    /// A finished day with 90% to 98% covered. Use it only when the cloud has
    /// nothing for the day.
    Partial,
    /// Good enough to win over the cloud counters.
    Solid,
}

/// How well do the live samples cover this day? `covered_secs` is the seconds
/// the samples cover, `[start, end)` is the local day and `now` is the current
/// time. A finished day is solid at 98% of its length. Today is solid at 90%
/// of the time since midnight, so it can use live data before it is over.
fn live_coverage(covered_secs: i64, start: i64, end: i64, now: i64) -> LiveCoverage {
    let share = |min: f64, len: i64| len > 0 && covered_secs as f64 >= min * len as f64;
    let day = end - start;
    if now >= end {
        if share(MIN_FINISHED_LIVE_COVERAGE, day) {
            LiveCoverage::Solid
        } else if share(MIN_LIVE_COVERAGE, day) {
            LiveCoverage::Partial
        } else {
            LiveCoverage::Thin
        }
    } else if share(MIN_LIVE_COVERAGE, day)
        || (start <= now && share(MIN_LIVE_COVERAGE, now - start))
    {
        LiveCoverage::Solid
    } else {
        LiveCoverage::Thin
    }
}

/// Energy of one local day from the live samples, or `None` if they do not
/// cover enough of it (see [`live_coverage`]). Check [`LiveTotals::usable`]
/// before using the result.
pub(super) fn live_totals(
    samples: &[Sample],
    start: i64,
    end: i64,
    now: i64,
) -> Option<LiveTotals> {
    // Each column is integrated alone, skipping rows where it is missing.
    let column = |get: fn(&Sample) -> Option<f64>| {
        let pairs: Vec<(i64, f64)> = samples
            .iter()
            .filter_map(|s| Some((s.ts, get(s)?)))
            .collect();
        integrate(&pairs)
    };
    let pv = column(|s| s.pv_w);
    let load = column(|s| s.load_w);
    let grid = column(|s| s.grid_w);
    let battery = column(|s| s.battery_w);
    // The least-covered column decides, so no total rests on thin data.
    let covered = [&pv, &load, &grid, &battery]
        .iter()
        .map(|i| i.covered_secs)
        .min()?;
    let coverage = live_coverage(covered, start, end, now);
    (coverage != LiveCoverage::Thin).then_some(LiveTotals {
        solid: coverage == LiveCoverage::Solid,
        pv_kwh: pv.positive_kwh,
        load_kwh: load.positive_kwh,
        import_kwh: grid.positive_kwh,
        export_kwh: grid.negative_kwh,
        battery_charge_kwh: battery.positive_kwh,
        battery_discharge_kwh: battery.negative_kwh,
    })
}

pub(super) struct LiveTotals {
    /// Covers the day well enough to win over the cloud counters.
    pub(super) solid: bool,
    pv_kwh: f64,
    load_kwh: f64,
    import_kwh: f64,
    export_kwh: f64,
    battery_charge_kwh: f64,
    battery_discharge_kwh: f64,
}

impl LiveTotals {
    /// Use these totals rather than the cloud's? Always when solid; a partial
    /// day only when the cloud has no data for it, so the day is not blanked.
    pub(super) fn usable(&self, cloud_has_data: bool) -> bool {
        self.solid || !cloud_has_data
    }
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
    /// Where the numbers come from: "inverter" (live samples) or "cloud".
    source: &'static str,
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

/// `live_range` is the oldest and newest live sample time (see
/// `db::live_ts_range`), so days without any live data skip the extra query.
async fn day_totals(
    state: &AppState,
    date: NaiveDate,
    live_range: Option<(i64, i64)>,
    now: i64,
) -> Result<DayTotals, ApiError> {
    // Each day gets its own [start, end), so a 23 or 25 hour day is handled correctly.
    let (start, end) = day_bounds_or_error(date, state.tz)?;
    let live = match live_range {
        Some((first, last)) if first < end && last >= start => {
            let samples = db::live_samples_raw(&state.pool, start, end).await?;
            live_totals(&samples, start, end, now)
        }
        _ => None,
    };
    // A solid live day never needs the cloud; skip its queries.
    if let Some(live) = live.as_ref().filter(|live| live.solid) {
        return Ok(live_day(date, live));
    }
    let cloud = cloud_day(state, date, start, end).await?;
    match live {
        Some(live) if live.usable(cloud.has_data()) => Ok(live_day(date, &live)),
        _ => Ok(cloud),
    }
}

/// A day's totals from the live samples.
fn live_day(date: NaiveDate, live: &LiveTotals) -> DayTotals {
    DayTotals {
        date: date.to_string(),
        source: "inverter",
        pv_kwh: Some(live.pv_kwh),
        load_kwh: Some(live.load_kwh),
        import_kwh: Some(live.import_kwh),
        export_kwh: Some(live.export_kwh),
        battery_charge_kwh: Some(live.battery_charge_kwh),
        battery_discharge_kwh: Some(live.battery_discharge_kwh),
        // Needs a pv-to-battery split we do not have; the cloud's value
        // would not match the live numbers, so leave it out.
        self_consumed_kwh: None,
    }
}

/// A day's totals from the iSolarCloud counters.
async fn cloud_day(
    state: &AppState,
    date: NaiveDate,
    start: i64,
    end: i64,
) -> Result<DayTotals, ApiError> {
    Ok(DayTotals {
        date: date.to_string(),
        source: "cloud",
        pv_kwh: day_max_kwh(state, 83022, start, end).await?,
        load_kwh: day_max_kwh(state, 83118, start, end).await?,
        import_kwh: day_max_kwh(state, 83102, start, end).await?,
        export_kwh: day_max_kwh(state, 83072, start, end).await?,
        battery_charge_kwh: day_max_kwh(state, 83322, start, end).await?,
        battery_discharge_kwh: day_max_kwh(state, 83323, start, end).await?,
        self_consumed_kwh: day_max_kwh(state, 83097, start, end).await?,
    })
}

impl DayTotals {
    /// Does the day have any figure at all?
    fn has_data(&self) -> bool {
        [
            self.pv_kwh,
            self.load_kwh,
            self.import_kwh,
            self.export_kwh,
            self.battery_charge_kwh,
            self.battery_discharge_kwh,
            self.self_consumed_kwh,
        ]
        .iter()
        .any(Option::is_some)
    }
}

/// Reads the optional `from` and `to` dates (default: the last
/// `default_days` days ending today) and checks the range. Gives the
/// first date and how many days the range has, at most `max_days`.
pub(super) fn date_range(
    from: Option<&str>,
    to: Option<&str>,
    tz: Tz,
    max_days: u64,
    default_days: u64,
) -> Result<(NaiveDate, u64), ApiError> {
    let to = match to {
        Some(text) => parse_date("to", text)?,
        None => today(tz),
    };
    let from = match from {
        Some(text) => parse_date("from", text)?,
        None => to - Days::new(default_days - 1),
    };
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    let count = (to - from).num_days() as u64 + 1;
    if count > max_days {
        return Err(ApiError::bad_request(format!(
            "the range can be at most {max_days} days"
        )));
    }
    Ok((from, count))
}

pub async fn daily(State(state): State<AppState>, Query(params): Query<DailyParams>) -> ApiResult {
    let (from, count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        MAX_DAILY_DAYS,
        DEFAULT_DAILY_DAYS,
    )?;

    let live_range = db::live_ts_range(&state.pool).await?;
    let now = db::now();
    let mut days = Vec::new();
    for date in from.iter_days().take(count as usize) {
        days.push(day_totals(&state, date, live_range, now).await?);
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
    fn soc_is_shown_as_a_percentage() {
        assert!((display_value(SOC_POINT, 0.674) - 67.4).abs() < 1e-9);
        assert_eq!(display_value(83033, 1500.0), 1500.0);
    }

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
        let saturday = day_totals(&state, date("2026-04-04"), None, 0)
            .await
            .unwrap();
        let sunday = day_totals(&state, date("2026-04-05"), None, 0)
            .await
            .unwrap();
        let monday = day_totals(&state, date("2026-04-06"), None, 0)
            .await
            .unwrap();
        assert_eq!(saturday.pv_kwh, Some(100.0));
        assert_eq!(sunday.pv_kwh, Some(9.0));
        assert_eq!(monday.pv_kwh, Some(0.0));
        // No data for the other counters.
        assert_eq!(sunday.load_kwh, None);
        assert_eq!(sunday.source, "cloud");
    }

    #[test]
    fn integrates_steady_power() {
        // 1000 W with a sample every minute for an hour: 60 steps of 60 s.
        let samples: Vec<_> = (0..=60).map(|i| (i * 60, 1000.0)).collect();
        let got = integrate(&samples);
        assert_eq!(got.covered_secs, 3600);
        assert!((got.positive_kwh - 1.0).abs() < 1e-9);
        assert_eq!(got.negative_kwh, 0.0);
    }

    #[test]
    fn integration_caps_gaps_and_splits_signs() {
        // 0..15: +1000 W (15 s). 15..30: -2000 W (15 s).
        // 30..330 is a 300 s gap: only 120 s of the -500 W counts.
        // 330 is the last sample and adds nothing.
        let samples = [(0, 1000.0), (15, -2000.0), (30, -500.0), (330, 800.0)];
        let got = integrate(&samples);
        assert_eq!(got.covered_secs, 15 + 15 + 120);
        // 1000 * 15 = 15,000 Ws.
        assert!((got.positive_kwh - 15_000.0 / 3.6e6).abs() < 1e-12);
        // 2000 * 15 + 500 * 120 = 90,000 Ws.
        assert!((got.negative_kwh - 90_000.0 / 3.6e6).abs() < 1e-12);
        // Fewer than two samples cover nothing.
        assert_eq!(integrate(&[(0, 5.0)]).covered_secs, 0);
        assert_eq!(integrate(&[]).positive_kwh, 0.0);
    }

    #[test]
    fn live_is_used_when_coverage_is_high_enough() {
        use LiveCoverage::{Partial, Solid, Thin};
        let (start, end) = (0, 86_400);
        let later = 1_000_000; // a later day: the day is finished
        // A finished day is solid from 98% (84,672 s).
        assert_eq!(live_coverage(84_672, start, end, later), Solid);
        assert_eq!(live_coverage(84_671, start, end, later), Partial);
        assert_eq!(live_coverage(77_760, start, end, later), Partial); // exactly 90%
        assert_eq!(live_coverage(77_759, start, end, later), Thin);
        // Today at 06:00: 90% of the 6 hours so far is enough, and solid.
        let now = 6 * 3600;
        assert_eq!(live_coverage(19_440, start, end, now), Solid);
        assert_eq!(live_coverage(19_439, start, end, now), Thin);
        // A future day has nothing elapsed, so it never uses live.
        assert_eq!(live_coverage(0, start, end, -1), Thin);
    }

    /// Live totals for a day, with or without cloud data to fall back on.
    fn totals(solid: bool) -> LiveTotals {
        LiveTotals {
            solid,
            pv_kwh: 0.0,
            load_kwh: 0.0,
            import_kwh: 0.0,
            export_kwh: 0.0,
            battery_charge_kwh: 0.0,
            battery_discharge_kwh: 0.0,
        }
    }

    #[test]
    fn a_partial_live_day_gives_way_to_the_cloud_only_when_it_has_data() {
        // 95% live with cloud present: the cloud wins.
        assert!(!totals(false).usable(true));
        // 95% live and no cloud: keep the live numbers, do not blank the day.
        assert!(totals(false).usable(false));
        // 99% live: live wins either way.
        assert!(totals(true).usable(true));
        assert!(totals(true).usable(false));
    }

    #[test]
    fn live_totals_marks_95_percent_partial_and_99_percent_solid() {
        // One column of constant samples every 60 s with a hold of 120 s
        // covers each step fully, so the share is set by where they stop.
        let samples = |last_ts: i64| -> Vec<Sample> {
            (0..=last_ts / 60)
                .map(|i| Sample {
                    ts: i * 60,
                    pv_w: Some(1000.0),
                    load_w: Some(500.0),
                    grid_w: Some(0.0),
                    battery_w: Some(0.0),
                    soc_pct: None,
                })
                .collect()
        };
        let (start, end, now) = (0, 86_400, 1_000_000);
        let partial = live_totals(&samples(82_080), start, end, now).unwrap(); // 95%
        assert!(!partial.solid);
        let solid = live_totals(&samples(85_500), start, end, now).unwrap(); // 99%
        assert!(solid.solid);
        assert!(live_totals(&samples(70_000), start, end, now).is_none());
    }

    #[tokio::test]
    async fn live_answers_from_memory_once_the_poller_has_a_sample() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let sample = Sample {
            ts: db::now() - 5,
            pv_w: Some(5000.0),
            load_w: Some(700.0),
            grid_w: Some(-300.0),
            battery_w: Some(4000.0),
            soc_pct: Some(67.4),
        };
        // Only in memory, not in the database.
        state.latest().set(sample);
        let Json(json) = live(State(state.clone())).await.unwrap();
        assert_eq!(json["sample"]["ts"], sample.ts);
        assert_eq!(json["sample"]["soc_pct"], 67.4);
        // TeslaMate is off, so there is no car.
        assert!(json["car"].is_null());

        // A sample that has gone stale in memory is still not "live".
        state.latest().set(Sample {
            ts: db::now() - 600,
            ..sample
        });
        let Json(json) = live(State(state)).await.unwrap();
        assert!(json["sample"].is_null());
    }

    #[tokio::test]
    async fn daily_prefers_live_samples_when_they_cover_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        // Cloud counter for both days: 5 kWh.
        let point = Point {
            point_id: 83102,
            code: None,
            name: None,
            unit: Some("Wh".to_string()),
        };
        db::upsert_point(&pool, &point).await.unwrap();
        let state_day = |date: &str| {
            let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
            day_bounds(date, ADELAIDE).unwrap()
        };
        let (start, end) = state_day("2026-10-19");
        let (start2, _) = state_day("2026-10-20");
        let cloud: Vec<Reading> = [start + 3600, start2 + 3600]
            .iter()
            .map(|&ts| Reading {
                point_id: 83102,
                ts,
                value: Some(5000.0),
            })
            .collect();
        db::upsert_readings(&pool, 7, &cloud).await.unwrap();

        // The 19th has a sample every minute all day: import 1000 W (1 kWh/h),
        // export 0, so 24 kWh minus the last minute, which has no next sample.
        for ts in (start..end).step_by(60) {
            let sample = Sample {
                ts,
                pv_w: Some(0.0),
                load_w: Some(1000.0),
                grid_w: Some(1000.0),
                battery_w: Some(0.0),
                soc_pct: Some(50.0),
            };
            db::insert_live_sample(&pool, &sample).await.unwrap();
        }

        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let range = db::live_ts_range(&state.pool).await.unwrap();
        let now = start2 + 12 * 3600;

        let live = day_totals(&state, date("2026-10-19"), range, now)
            .await
            .unwrap();
        assert_eq!(live.source, "inverter");
        // 1439 steps of 60 s at 1000 W.
        let expected = 1000.0 * 1439.0 * 60.0 / 3.6e6;
        assert!((live.import_kwh.unwrap() - expected).abs() < 1e-9);
        assert_eq!(live.export_kwh, Some(0.0));
        assert_eq!(live.self_consumed_kwh, None);

        // The 20th has cloud data but no live samples.
        let cloud_day = day_totals(&state, date("2026-10-20"), range, now)
            .await
            .unwrap();
        assert_eq!(cloud_day.source, "cloud");
        assert_eq!(cloud_day.import_kwh, Some(5.0));
    }
}
