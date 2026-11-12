//! The JSON API: handlers, input checks and the derivation of power from
//! the daily energy counters.

use std::collections::HashMap;

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
use super::costs::{
    Group, MAX_GROUPED_COSTS_DAYS, cache_key, counter_lag_secs, day_grid_split_checked,
    is_cacheable, round2, undo_counter_lag,
};
use super::day_cache;
use super::home_mix::{self, HomeMix};
use super::samples::{self, Role};
use crate::amber::store as amber_store;
use crate::cost::{GridSplit, TariffSchedule};
use crate::db;
use crate::ev_cost;
use crate::ha_source::samples as ha_samples;
use crate::hub::{SampleSource, SourceRules};
use crate::isolar::points;
use crate::metric::Metric;
use crate::modbus::{self, Sample};

const MAX_SERIES_POINTS: usize = 20;
const MAX_SERIES_SECS: i64 = 31 * 24 * 3600;
/// `/api/live` ignores a sample older than this: it would show stale data as current.
const LIVE_MAX_AGE_SECS: i64 = 5 * 60;
const MAX_LIVE_SERIES_SECS: i64 = 2 * 24 * 3600;
const DEFAULT_INVERTER_HISTORY_SECS: i64 = 7 * 24 * 3600;
const MAX_INVERTER_HISTORY_SECS: i64 = 400 * 24 * 3600;
const MAX_INVERTER_HISTORY_ROWS: i64 = 500;
const MAX_DAILY_DAYS: u64 = 400;
const MAX_EV_SESSIONS_SECS: i64 = MAX_DAILY_DAYS as i64 * 24 * 3600;
/// Without `from`, `/api/daily` returns this many days.
pub(super) const DEFAULT_DAILY_DAYS: u64 = 30;
/// Without `from`, `/api/ev/summary` covers this many days (about 12 months).
const DEFAULT_SUMMARY_DAYS: u64 = 365;
/// The longest time one counter step is spread over (see `derive_power`).
pub(super) const MAX_SPREAD_SECS: i64 = 30 * 60;
/// The size of one step of an iSolarCloud daily counter, in Wh.
const QUANTUM_WH: f64 = 100.0;
/// The longest time one live sample is held (see `integrate`). Samples come
/// every ~15 s, so a longer step means the dongle dropped out.
pub(super) const MAX_HOLD_SECS: i64 = 120;

/// Power metrics shown by `/api/day`: (key, metric).
const POWER_METRICS: [(&str, Metric); 3] = [
    ("pv_w", Metric::PvW),
    ("load_w", Metric::LoadW),
    ("soc", Metric::SocPct),
];

/// Daily counters (Wh) that `/api/day` turns into power:
/// (key, counter metric, made-up id for the derived series, name).
const DERIVED_METRICS: [(&str, Metric, i64, &str); 4] = [
    (
        "grid_import_w",
        Metric::GridImportDayWh,
        -1,
        "Grid import power",
    ),
    (
        "grid_export_w",
        Metric::GridExportDayWh,
        -2,
        "Grid export power",
    ),
    (
        "battery_charge_w",
        Metric::BatteryChargeDayWh,
        -3,
        "Battery charge power",
    ),
    (
        "battery_discharge_w",
        Metric::BatteryDischargeDayWh,
        -4,
        "Battery discharge power",
    ),
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

    pub(super) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
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
    /// The metric's name, for a series that is a stored metric (not a derived one).
    #[serde(skip_serializing_if = "Option::is_none")]
    metric: Option<&'static str>,
    /// Deprecated: the data source's own id. Use `metric`.
    point_id: i64,
    name: Option<String>,
    unit: Option<String>,
    /// `[ts, value]` pairs, oldest first.
    data: Vec<(i64, f64)>,
}

/// Loads one point, named by its source id, as a series. The store is keyed
/// by metric, so the point id is mapped to its metric first; a point with no
/// metric has no stored readings and gives an empty series.
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
    let data = match points::for_point(point_id) {
        Some(map) => {
            db::readings_in_range(&state.pool, state.plant_id, map.metric, from, to).await?
        }
        None => Vec::new(),
    };
    Ok(Series {
        key,
        metric: points::for_point(point_id).map(|map| map.metric.name()),
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

/// The spokes (background data sources and sinks) and whether each is on.
pub async fn spokes(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "spokes": &*state.spokes }))
}

/// What this install can do, from the sources that run and what has been
/// entered. The UI hides screens that would be empty. The DB-backed answers
/// (costs, cars, payback) are worked out per request, so they follow the admin
/// page.
pub async fn capabilities(State(state): State<AppState>) -> ApiResult {
    let enabled = |id: &str| state.spokes.iter().any(|s| s.id == id && s.enabled);
    // Home Assistant supplies the live view or history only when the rules
    // say so (and its source is running); see `[sources]`.
    let ha_on = enabled("home-assistant");
    let modbus_live = state.rules.live_order().contains(&SampleSource::Modbus);
    let live = (enabled("sungrow-modbus") && modbus_live) || (ha_on && state.rules.ha_live());
    let history = enabled("isolarcloud") || (ha_on && state.rules.ha_daily());
    let meter = state.globird_configured || enabled("globird-portal");
    let prices = state.amber_configured || enabled("amber");
    let admin = state.admin.get();
    let costs = admin.tariff.is_some();
    let payback = costs && !admin.purchases.is_empty();
    let teslamate = enabled("teslamate-mqtt") || enabled("teslamate-history");
    let manual_cars: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM manual_car")
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    let sources: Vec<&str> = state
        .spokes
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.id)
        .collect();
    Ok(Json(json!({
        "live": live,
        "history": history,
        "meter": meter,
        "prices": prices,
        "costs": costs,
        "cars": teslamate || manual_cars > 0,
        "payback": payback,
        "meter_name": if meter { Some("GloBird") } else { None },
        "sources": sources,
    })))
}

pub async fn latest(State(state): State<AppState>) -> ApiResult {
    let mut rows = db::latest_metric_readings(&state.pool, state.plant_id).await?;
    // The same order as before the store was keyed by metric: by point id.
    rows.sort_by_key(|(metric, _, _)| points::for_metric(*metric).point_id);
    let mut out: Vec<Value> = Vec::new();
    for (metric, ts, value) in rows {
        let point_id = points::for_metric(metric).point_id;
        let (name, unit) = db::point_info(&state.pool, point_id)
            .await?
            .unwrap_or((None, None));
        // A point row made only as a placeholder has no unit; the metric knows its own.
        let unit = unit.unwrap_or_else(|| metric.unit().to_string());
        out.push(json!({
            "metric": metric.name(),
            // Deprecated: the data source's own id. Use `metric`.
            "point_id": point_id,
            "name": name,
            "unit": unit,
            "ts": ts,
            "value": value,
        }));
    }
    Ok(Json(json!({ "points": out })))
}

/// `{"sample": ..., "car": ..., "grid_pull": ...}`. The sample is the newest
/// Modbus one, or null if there is none or it is too old to call "live" (for
/// example when Modbus is off). It comes from memory; the database is only
/// read until the poller's first sample after a restart.
///
/// The car is TeslaMate's last known state, or null if TeslaMate is off or
/// has sent nothing yet. It gets no age limit: TeslaMate's values are
/// retained and stay true while the car sleeps. `car.updated_ts` says how
/// old they are.
///
/// `grid_pull` is how hard the house is drawing from the grid, for watching
/// the main breaker (see [`grid_pull_json`]).
pub async fn live(State(state): State<AppState>) -> ApiResult {
    // The sources of `[sources] live`, best first: the first with a fresh
    // sample wins. By default that is Modbus alone.
    let mut fresh = None;
    let mut from_ha = false;
    for source in state.rules.live_order() {
        match source {
            SampleSource::Modbus => {
                let sample = match state.latest.get() {
                    Some(sample) => Some(sample),
                    None => db::latest_live_sample(&state.pool).await?,
                };
                fresh = sample.filter(|s| s.ts >= db::now() - LIVE_MAX_AGE_SECS);
            }
            SampleSource::HomeAssistant => {
                if let Some(ha) = state.rules.ha() {
                    fresh = ha_samples::latest(&state.pool, ha, db::now()).await?;
                    from_ha = fresh.is_some();
                }
            }
        }
        if fresh.is_some() {
            break;
        }
    }
    let since = local_midnight(today(state.tz), state.tz).unwrap_or(0);
    // The peak comes from the source of the sample (or, with no sample, from
    // the preferred source).
    let peak_from_ha = from_ha
        || (fresh.is_none()
            && state.rules.live_order().first() == Some(&SampleSource::HomeAssistant));
    let peak = match state.rules.ha() {
        Some(ha) if peak_from_ha => ha_samples::peak_import(&state.pool, ha, since).await?,
        _ => db::peak_grid_import(&state.pool, since).await?,
    };
    let data = state.admin.get();
    let grid_pull = grid_pull_json(
        fresh.as_ref(),
        peak,
        data.grid_pull_warn_kw,
        data.grid_pull_limit_kw,
    );
    Ok(Json(
        json!({ "sample": fresh, "car": state.car.get(), "grid_pull": grid_pull }),
    ))
}

/// The `grid_pull` part of `/api/live`:
/// `{now_w, peak_today_w, peak_today_at, warn_w, limit_w, volts}`.
///
/// `now_w` is the grid import of the fresh sample in W. `Sample::grid_w` is
/// positive when importing and negative when exporting, so exporting counts
/// as 0 pulled. It is null when there is no fresh sample. `peak` is the
/// highest import so far today as `(ts, W)`, or None. `volts` is null: the
/// Modbus poller does not read the grid voltage yet.
fn grid_pull_json(
    sample: Option<&Sample>,
    peak: Option<(i64, f64)>,
    warn_kw: f64,
    limit_kw: f64,
) -> Value {
    let now_w = sample
        .and_then(|s| s.grid_w)
        .map(|w| w.max(0.0).round() as i64);
    json!({
        "now_w": now_w,
        "peak_today_w": peak.map(|(_, w)| w.round() as i64),
        "peak_today_at": peak.map(|(ts, _)| ts),
        "warn_w": (warn_kw * 1000.0).round() as i64,
        "limit_w": (limit_kw * 1000.0).round() as i64,
        "volts": Value::Null,
    })
}

/// `{"settings": ...}`: the inverter's control settings with their labels, or
/// null if there are none yet or they are too old (for example when Modbus is
/// off or the dongle has been unreachable). The raw register values are kept
/// next to the decoded ones.
pub async fn inverter(State(state): State<AppState>) -> ApiResult {
    let fresh = state
        .latest_settings
        .get()
        .filter(|s| s.ts >= db::now() - LIVE_MAX_AGE_SECS);
    let settings = match fresh {
        Some(s) => settings_json(&s)?,
        None => Value::Null,
    };
    Ok(Json(json!({ "settings": settings })))
}

/// The settings with the labels for their raw register values added.
fn settings_json(s: &modbus::Settings) -> Result<Value, ApiError> {
    let mut json = serde_json::to_value(s).map_err(anyhow::Error::from)?;
    json["ems_mode_label"] = json!(s.ems_mode.map(modbus::ems_mode_label));
    json["forced_command_label"] = json!(s.forced_command.map(modbus::forced_command_label));
    json["running_state_label"] = json!(s.running_state.map(modbus::running_state_label));
    Ok(json)
}

#[derive(Deserialize)]
pub struct InverterHistoryParams {
    from: Option<String>,
    to: Option<String>,
}

/// `{"changes": [...], "previous": {...}}`: the times the inverter's settings
/// changed, newest first, each with the settings from then on and their
/// labels. `previous` is the newest row before `from` (the setting in force
/// when the range starts), or null. `from`
/// (inclusive) and `to` (exclusive) are unix seconds and default to the last
/// 7 days.
pub async fn inverter_history(
    State(state): State<AppState>,
    Query(params): Query<InverterHistoryParams>,
) -> ApiResult {
    let now = db::now();
    let to = match params.to.as_deref() {
        Some(t) => parse_unix("to", Some(t))?,
        None => now + 1,
    };
    let from = match params.from.as_deref() {
        Some(t) => parse_unix("from", Some(t))?,
        None => to - DEFAULT_INVERTER_HISTORY_SECS,
    };
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_INVERTER_HISTORY_SECS {
        return Err(ApiError::bad_request("the range can be at most 400 days"));
    }

    let rows =
        db::inverter_settings_in_range(&state.pool, from, to, MAX_INVERTER_HISTORY_ROWS).await?;
    let changes = rows
        .iter()
        .map(settings_json)
        .collect::<Result<Vec<Value>, ApiError>>()?;
    let previous = match db::inverter_settings_before(&state.pool, from).await? {
        Some(s) => settings_json(&s)?,
        None => Value::Null,
    };
    Ok(Json(json!({ "changes": changes, "previous": previous })))
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
    let samples = samples::per_minute(&state.pool, &state.rules, Role::Live, from, to + 1).await?;
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
    let data = state.admin.get();
    let Some(schedule) = &data.tariff else {
        return Ok(None);
    };
    // `to` is exclusive in the query, so add a second to include the end.
    let end = session.ended_at.unwrap_or(now);
    let samples = samples::raw(
        &state.pool,
        &state.rules,
        Role::Daily,
        session.started_at,
        end + 1,
    )
    .await?;
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
    /// Metric names, such as `pv_w,load_w`.
    metrics: Option<String>,
    /// Deprecated: the data source's own point ids. Use `metrics`.
    points: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

/// Parses `pv_w,load_w` into metrics.
fn parse_metrics(text: &str) -> Result<Vec<Metric>, ApiError> {
    let metrics: Vec<Metric> = text
        .split(',')
        .map(|part| {
            let name = part.trim();
            Metric::from_name(name)
                .ok_or_else(|| ApiError::bad_request(format!("{name:?} is not a metric name")))
        })
        .collect::<Result<_, _>>()?;
    if metrics.len() > MAX_SERIES_POINTS {
        return Err(ApiError::bad_request(format!(
            "metrics must have between 1 and {MAX_SERIES_POINTS} names"
        )));
    }
    Ok(metrics)
}

/// Parses a comma-separated list of point ids (the deprecated way to ask).
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
    // `metrics` is the way to ask. `points` (numeric ids) still works for one
    // release; the answer says so.
    let (point_ids, deprecated) = match (params.metrics.as_deref(), params.points.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request("use metrics or points, not both"));
        }
        (Some(text), None) => {
            let ids = parse_metrics(text)?
                .into_iter()
                .map(|metric| points::for_metric(metric).point_id)
                .collect();
            (ids, false)
        }
        (None, text) => (parse_points(text.unwrap_or(""))?, true),
    };
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_SERIES_SECS {
        return Err(ApiError::bad_request("the range can be at most 31 days"));
    }

    let mut series = Vec::new();
    for point_id in point_ids {
        // `to` is inclusive here; the query's end is exclusive.
        series.push(stored_series(&state, None, point_id, from, to + 1).await?);
    }
    let mut answer = json!({ "series": series });
    if deprecated {
        answer["deprecated"] = json!("points is deprecated and will be removed; ask with metrics");
    }
    Ok(Json(answer))
}

/// Charging, km and petrol for each month of the range, as
/// `{"configured":bool,"months":[{month,km,home_kwh,home_cost,away_kwh,away_cost,charging_cost,petrol_litres,petrol_cost,saved,ev_cents_per_km,petrol_cents_per_km},...],"total":{...},"petrol_cents_per_litre":..,"petrol_litres_per_100km":..}`.
/// The dates are local, `from` and `to` included, at most 400 days; without
/// `from` it covers the last [`DEFAULT_SUMMARY_DAYS`] days. Money is in dollars.
///
/// km come from the daily odometer readings, and each session counts in the
/// month it started. Home sessions are costed like `/api/ev/sessions` does;
/// away sessions at `away_cents_per_kwh` (see `ev_cost::summarise`).
/// Petrol is priced for each month from the petrol price history, like
/// `/api/ev/cars` does. `configured` is false, and the money fields are null,
/// without a `petrol_cents_per_litre` or any monthly petrol price.
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

    let data = state.admin.get();
    let prices = data.ev_prices;
    let petrol = &data.petrol;
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
        } else if petrol.is_configured() {
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

    let summary = ev_cost::summarise(&prices, petrol, &km_by_month, &sessions);
    Ok(Json(json!({
        "configured": summary.configured,
        "months": summary.months,
        "total": summary.total,
        "petrol_cents_per_litre": prices.petrol_cents_per_litre,
        "petrol_litres_per_100km": prices.petrol_litres_per_100km,
    })))
}

/// Each TeslaMate car's fuel savings over its whole history, newest car
/// first: `{"configured":bool,"petrol_litres_per_100km":..,"cars":[{car_id,name,model,label,first_day,last_day,total:{...},months:[{month,...},...]},...]}`.
/// `configured` is true with a `petrol_cents_per_litre`, any
/// `[[teslamate.petrol_price]]` or any entry in the `petrol_prices_file`. Money is in dollars. Each
/// period has the km, petrol litres and cost (each day at that day's price),
/// the home and away kWh, and the charging cost and saving `with_system` (as
/// the home sessions were costed) and `without_system` (home charging at the
/// baseline), with `system_added` the difference; see `ev_cost::CarPeriod`.
///
/// The numbers come from the synced `tm_*` tables (the Postgres history
/// sync), not the live MQTT data, so the list is empty until that has run.
/// Sessions count in the local month they started, and only the home ones
/// (geofence "Home") are costed from the house's energy; home sessions not
/// costed yet are counted in `pending_sessions` and left out of the money.
/// `label` is "Model <model>", else the car's name, else "Car <id>", with the
/// name added when two cars would share a label. Cars with no drives and no
/// charges are left out. Each car also has `owned_since`, the day it was
/// bought (YYYY-MM-DD) as set on the Admin page, or null; TeslaMate has no
/// purchase date.
pub async fn ev_cars(State(state): State<AppState>) -> ApiResult {
    let data = state.admin.get();
    let prices = data.ev_prices;
    let petrol = &data.petrol;
    let owned = db::tm_car_owned(&state.pool).await?;
    let mut cars = Vec::new();
    for car in db::tm_cars(&state.pool).await? {
        let days: Vec<ev_cost::CarDay> = db::tm_drive_days(&state.pool, car.id)
            .await?
            .into_iter()
            .map(|day| ev_cost::CarDay {
                date: day.date,
                km: day.km,
            })
            .collect();
        let charges: Vec<ev_cost::CarCharge> = db::tm_charge_rows(&state.pool, car.id)
            .await?
            .into_iter()
            .filter_map(|row| car_charge(&row, state.tz))
            .collect();
        let history = ev_cost::summarise_car(&prices, petrol, &days, &charges);
        if history.first_day.is_some() {
            cars.push((car, history, charges));
        }
    }
    // Newest car first.
    cars.sort_by(|a, b| {
        b.1.first_day
            .cmp(&a.1.first_day)
            .then_with(|| a.0.id.cmp(&b.0.id))
    });
    let labels: Vec<String> = cars.iter().map(|(car, _, _)| car_label(car)).collect();
    let (plans, plan_of) = charging_plans(data.tariff.as_deref());
    let summarise = |charges: &[ev_cost::CarCharge]| {
        ev_cost::summarise_charging(&prices, &plans, &plan_of, charges)
    };
    let all_charges: Vec<ev_cost::CarCharge> = cars
        .iter()
        .flat_map(|(_, _, charges)| charges.iter().cloned())
        .collect();
    let combined = summarise(&all_charges);
    let cars: Vec<Value> = cars
        .iter()
        .zip(&labels)
        .map(|((car, history, charges), label)| {
            let shared = labels.iter().filter(|other| *other == label).count() > 1;
            let label = match (&car.name, shared) {
                (Some(name), true) => format!("{label} ({name})"),
                _ => label.clone(),
            };
            let summary = summarise(charges);
            json!({
                "car_id": car.id,
                "source": "teslamate",
                "name": car.name,
                "model": car.model,
                "label": label,
                "owned_since": owned.get(&car.id),
                "first_day": history.first_day,
                "last_day": history.last_day,
                "total": history.total,
                "months": history.months,
                "trim": car.trim_badging,
                "paint": car.exterior_color,
                "wheels": car.wheel_type,
                "marketing_name": car.marketing_name,
                "compare_with": ev_cost::compare_with(&data.compare_cars, car.model.as_deref()),
                "charging_by_plan": summary.charging_by_plan,
                "away": summary.away,
                "self_share_pct": summary.self_share_pct,
            })
        })
        .collect();
    Ok(Json(json!({
        "configured": petrol.is_configured(),
        "petrol_litres_per_100km": prices.petrol_litres_per_100km,
        "petrol_grade": data.petrol_grade,
        "charging_by_plan": combined.charging_by_plan,
        "away": combined.away,
        "self_share_pct": combined.self_share_pct,
        "cars": cars,
    })))
}

/// The plan names for `charging_by_plan`, oldest first, and which of them a
/// local day ("YYYY-MM-DD") falls on. Without a `[tariff]` there is one plan,
/// "Home charging" (nothing is costed then anyway).
fn charging_plans(schedule: Option<&TariffSchedule>) -> (Vec<String>, impl Fn(&str) -> usize + '_) {
    let plans = match schedule {
        Some(schedule) => schedule
            .plan_spans()
            .iter()
            .map(|p| ev_cost::plan_label(p.name.as_deref(), p.from, p.until))
            .collect(),
        None => vec!["Home charging".to_string()],
    };
    let plan_of = move |day: &str| match (schedule, NaiveDate::parse_from_str(day, "%Y-%m-%d")) {
        (Some(schedule), Ok(date)) => schedule.plan_index(date),
        _ => 0,
    };
    (plans, plan_of)
}

/// "Model 3" for a car with a model, else its name, else "Car 1".
fn car_label(car: &db::TmCar) -> String {
    let set = |text: &Option<String>| text.clone().filter(|t| !t.trim().is_empty());
    match (set(&car.model), set(&car.name)) {
        (Some(model), _) => format!("Model {model}"),
        (None, Some(name)) => name,
        (None, None) => format!("Car {}", car.id),
    }
}

/// A stored session as the savings summary adds it up, or `None` if it took
/// no energy. Only a session at "Home" with all its cost columns is costed.
pub(super) fn car_charge(row: &db::TmChargeRow, tz: Tz) -> Option<ev_cost::CarCharge> {
    let kwh = row
        .energy_used_kwh
        .or(row.energy_added_kwh)
        .filter(|kwh| *kwh > 0.0)?;
    let started = DateTime::from_timestamp(row.started_at, 0)?;
    let cost = match (row.cost_key.as_ref(), row.cost_cents, row.baseline_cents) {
        (Some(_), Some(cost_cents), Some(baseline_cents)) => Some(ev_cost::HistoricCost {
            kwh,
            grid_kwh: row.grid_kwh.unwrap_or(0.0),
            self_kwh: row.self_kwh.unwrap_or(0.0),
            uncovered_kwh: row.uncovered_kwh.unwrap_or(0.0),
            cost_cents,
            baseline_cents,
            feed_in_forgone_cents: row.feed_in_forgone_cents.unwrap_or(0.0),
        }),
        _ => None,
    };
    Some(ev_cost::CarCharge {
        date: started.with_timezone(&tz).format("%Y-%m-%d").to_string(),
        kwh,
        at_home: row.geofence.as_deref() == Some("Home"),
        cost,
    })
}

pub(super) fn parse_date(name: &str, text: &str) -> Result<NaiveDate, ApiError> {
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
pub(super) fn day_bounds(date: NaiveDate, tz: Tz) -> Option<(i64, i64)> {
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
/// A rise of more than one 100 Wh step after a quiet spell is different: only
/// the first 100 Wh can have built up during the quiet spell. The rest came in
/// the last reading gap (for example a car starting to charge), so it is spread
/// over that gap only. Such a step gives two spreads.
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
    // The time of the reading before this one.
    let mut prev_ts = first_ts;

    for &(ts, value) in &counter[1..] {
        if value > last_value {
            let start = last_ts.max(ts - MAX_SPREAD_SECS);
            let wh = value - last_value;
            if wh > QUANTUM_WH && start < prev_ts {
                // One quantum over the quiet spell, the rest over the last gap.
                steps.push(Spread {
                    start,
                    end: prev_ts,
                    wh: QUANTUM_WH,
                });
                steps.push(Spread {
                    start: prev_ts,
                    end: ts,
                    wh: wh - QUANTUM_WH,
                });
            } else {
                steps.push(Spread { start, end: ts, wh });
            }
        }
        if value != last_value {
            (last_ts, last_value) = (ts, value);
        }
        prev_ts = ts;
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
/// time. With the default rules a finished day is solid at 98% of its length
/// and partial from 90%. Today is solid at 90% of the time since midnight, so
/// it can use live data before it is over. The shares come from `rules`.
fn live_coverage(
    covered_secs: i64,
    start: i64,
    end: i64,
    now: i64,
    rules: &SourceRules,
) -> LiveCoverage {
    let (finished, partial) = (rules.finished_day_coverage(), rules.partial_day_coverage());
    let share = |min: f64, len: i64| len > 0 && covered_secs as f64 >= min * len as f64;
    let day = end - start;
    if now >= end {
        if share(finished, day) {
            LiveCoverage::Solid
        } else if share(partial, day) {
            LiveCoverage::Partial
        } else {
            LiveCoverage::Thin
        }
    } else if share(partial, day) || (start <= now && share(partial, now - start)) {
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
    rules: &SourceRules,
) -> Option<LiveTotals> {
    if !rules.live_enabled() {
        return None;
    }
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
    let coverage = live_coverage(covered, start, end, now, rules);
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
    /// Use these totals rather than the cloud's? By default always when
    /// solid, and a partial day only when the cloud has no data for it, so
    /// the day is not blanked. `rules` can change which source is preferred.
    pub(super) fn usable(&self, rules: &SourceRules, cloud_has_data: bool) -> bool {
        rules.live_wins(self.solid, cloud_has_data)
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
    for (key, metric) in POWER_METRICS {
        let point_id = points::for_metric(metric).point_id;
        series.push(stored_series(&state, Some(key), point_id, start, end).await?);
    }
    for (key, counter_metric, derived_id, name) in DERIVED_METRICS {
        let counter =
            db::readings_in_range(&state.pool, state.plant_id, counter_metric, start, end).await?;
        series.push(Series {
            key: Some(key),
            metric: None,
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

#[derive(Deserialize, Clone)]
pub struct DailyParams {
    from: Option<String>,
    to: Option<String>,
    /// "day" (the default), "month" or "year": sum the days into one row per period.
    group: Option<String>,
}

/// Energy totals of one local day, in kWh. `None` means no data that day.
#[derive(Debug, Serialize, Deserialize)]
struct DayTotals {
    date: String,
    /// Where the numbers come from: "inverter" (live samples) or "cloud".
    source: std::borrow::Cow<'static, str>,
    pv_kwh: Option<f64>,
    load_kwh: Option<f64>,
    import_kwh: Option<f64>,
    export_kwh: Option<f64>,
    battery_charge_kwh: Option<f64>,
    battery_discharge_kwh: Option<f64>,
    self_consumed_kwh: Option<f64>,
    /// Grid energy by tariff band; all `null` without a tariff or interval data.
    #[serde(flatten)]
    split: SplitSums,
    /// The home load by source, solar first (see `home_mix`).
    #[serde(flatten)]
    home: HomeMix,
}

/// Grid energy by tariff band in kWh (see `cost::grid_split`), summed over
/// any number of days. A figure is `None` when no day had interval data.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct SplitSums {
    import_free_kwh: Option<f64>,
    import_shoulder_kwh: Option<f64>,
    import_peak_kwh: Option<f64>,
    export_super_kwh: Option<f64>,
    export_peak_kwh: Option<f64>,
    export_other_kwh: Option<f64>,
    export_tier_kwh: Option<f64>,
}

impl SplitSums {
    fn add(&mut self, split: &GridSplit) {
        fn add(sum: &mut Option<f64>, v: f64) {
            *sum = Some(sum.unwrap_or(0.0) + v);
        }
        add(&mut self.import_free_kwh, split.import_free_kwh);
        add(&mut self.import_shoulder_kwh, split.import_shoulder_kwh);
        add(&mut self.import_peak_kwh, split.import_peak_kwh);
        add(&mut self.export_super_kwh, split.export_super_kwh);
        add(&mut self.export_peak_kwh, split.export_peak_kwh);
        add(&mut self.export_other_kwh, split.export_other_kwh);
        add(&mut self.export_tier_kwh, split.export_tier_kwh);
    }

    fn merge(&mut self, other: &SplitSums) {
        fn add(sum: &mut Option<f64>, v: Option<f64>) {
            if let Some(v) = v {
                *sum = Some(sum.unwrap_or(0.0) + v);
            }
        }
        add(&mut self.import_free_kwh, other.import_free_kwh);
        add(&mut self.import_shoulder_kwh, other.import_shoulder_kwh);
        add(&mut self.import_peak_kwh, other.import_peak_kwh);
        add(&mut self.export_super_kwh, other.export_super_kwh);
        add(&mut self.export_peak_kwh, other.export_peak_kwh);
        add(&mut self.export_other_kwh, other.export_other_kwh);
        add(&mut self.export_tier_kwh, other.export_tier_kwh);
    }
}

/// Did any of the import, export and load counters (day maxima) rise above 0?
/// When iSolarCloud has a gap it still sends readings, all 0. A real day
/// always has a load counter above 0, so an all-zero day counts as no data.
pub(super) fn cloud_counters_rose(maxes: [Option<f64>; 3]) -> bool {
    maxes.iter().flatten().any(|&max| max > 0.0)
}

/// The day's total for one counter: its largest value within the local day,
/// because the counter only grows until it resets at midnight. Wh to kWh.
async fn day_max_kwh(
    state: &AppState,
    metric: Metric,
    start: i64,
    end: i64,
) -> Result<Option<f64>, ApiError> {
    let max = db::max_value_in_range_from(&state.pool, &state.counter_source(), metric, start, end)
        .await?;
    Ok(max.map(|wh| wh / 1000.0))
}

/// The `daily_cache` kind for a day's battery discharge.
const DISCHARGE_KIND: &str = "discharge";

/// Bump when the discharge a day reports would change for the same readings.
const DISCHARGE_VERSION: u32 = 1;

/// The cloud counters a day's totals read, used to tell a day before any data.
const CLOUD_DAY_METRICS: [Metric; 7] = [
    Metric::LoadDayWh,
    Metric::GridImportDayWh,
    Metric::GridExportDayWh,
    Metric::PvDayWh,
    Metric::BatteryChargeDayWh,
    Metric::BatteryDischargeDayWh,
    Metric::SelfConsumedDayWh,
];

/// The time of the first stored energy reading of any kind (the live samples
/// or a cloud counter), or `None` before any. A local day that ends at or
/// before it has no energy data, so its figures need no queries.
pub(super) async fn first_data_ts(state: &AppState) -> Result<Option<i64>, ApiError> {
    let first_live = samples::ts_range(&state.pool, &state.rules, Role::Daily)
        .await?
        .map(|(first, _)| first);
    let first_cloud =
        db::first_reading_ts_from(&state.pool, &state.counter_source(), &CLOUD_DAY_METRICS).await?;
    Ok([first_live, first_cloud].into_iter().flatten().min())
}

/// The battery's discharge in kWh on each of `dates` that has a figure, the
/// same one `/api/daily` reports (the live samples when they cover the day,
/// else the cloud counter). Days without a figure are left out.
///
/// A finished day with all its data is stored in `daily_cache`, so only days
/// that are not stored yet read their samples. A day that ends before the
/// first reading of any kind has no figure and costs no query.
pub(super) async fn battery_discharge_by_date(
    state: &AppState,
    dates: &[NaiveDate],
) -> Result<std::collections::BTreeMap<NaiveDate, f64>, ApiError> {
    let live_range = samples::ts_range(&state.pool, &state.rules, Role::Daily).await?;
    let now = db::now();
    let today = today(state.tz);
    let first_data = first_data_ts(state).await?;

    let mut keys = Vec::new();
    for &date in dates.iter().filter(|d| **d < today) {
        keys.push((
            date,
            format!("{DISCHARGE_VERSION}|{}|{:?}", state.tz, state.rules),
        ));
    }
    let mut stored: HashMap<NaiveDate, Option<f64>> = day_cache::load_with_data(
        &state.pool,
        state.plant_id,
        state.rules.uses_ha(),
        state.tz,
        DISCHARGE_KIND,
        &keys,
    )
    .await?;

    let mut to_store = Vec::new();
    for &date in dates {
        if stored.contains_key(&date) {
            continue;
        }
        let (_, end) = day_bounds_or_error(date, state.tz)?;
        // Nothing was recorded yet: every query below would come back empty.
        if first_data.is_none_or(|first| end <= first) {
            stored.insert(date, None);
            continue;
        }
        let (totals, _, final_day) = day_totals_flagged(state, date, live_range, now).await?;
        let discharge = totals.battery_discharge_kwh;
        // A finished day with gaps is stored too, keyed on its readings.
        if let Some((_, key)) = keys.iter().find(|(d, _)| *d == date) {
            to_store.push((date, key.clone(), final_day, discharge));
        }
        stored.insert(date, discharge);
    }
    let store_as: Vec<(NaiveDate, String, bool)> = to_store
        .iter()
        .map(|(date, key, complete, _)| (*date, key.clone(), *complete))
        .collect();
    let store_as = day_cache::store_keys(
        &state.pool,
        state.plant_id,
        state.rules.uses_ha(),
        state.tz,
        &store_as,
    )
    .await?;
    let rows: Vec<(NaiveDate, String, &Option<f64>)> = store_as
        .into_iter()
        .zip(&to_store)
        .map(|((date, key), (_, _, _, discharge))| (date, key, discharge))
        .collect();
    day_cache::save(&state.pool, DISCHARGE_KIND, &rows).await?;

    let mut found = std::collections::BTreeMap::new();
    for &date in dates {
        if let Some(Some(kwh)) = stored.get(&date) {
            found.insert(date, *kwh);
        }
    }
    Ok(found)
}

/// `live_range` is the oldest and newest live sample time (see
/// `db::live_ts_range`), so days without any live data skip the extra query.
#[cfg(test)]
async fn day_totals(
    state: &AppState,
    date: NaiveDate,
    live_range: Option<(i64, i64)>,
    now: i64,
) -> Result<DayTotals, ApiError> {
    Ok(day_totals_flagged(state, date, live_range, now).await?.0)
}

/// A day's totals and the live samples it loaded (empty when none, so the
/// tariff split can reuse them), and whether the day is final: finished, with all
/// its data (the test of [`is_cacheable`]), so a stored copy stays right.
async fn day_totals_flagged(
    state: &AppState,
    date: NaiveDate,
    live_range: Option<(i64, i64)>,
    now: i64,
) -> Result<(DayTotals, Vec<Sample>, bool), ApiError> {
    // Each day gets its own [start, end), so a 23 or 25 hour day is handled correctly.
    let (start, end) = day_bounds_or_error(date, state.tz)?;
    let samples = match live_range {
        Some((first, last)) if first < end && last >= start => {
            samples::raw(&state.pool, &state.rules, Role::Daily, start, end).await?
        }
        _ => Vec::new(),
    };
    let today = today(state.tz);
    let live = live_totals(&samples, start, end, now, &state.rules);
    let solid = live.as_ref().is_some_and(|l| l.solid);
    // A solid live day never needs the cloud (unless the rules prefer the
    // cloud, or there is no cloud source at all); skip its queries.
    if let Some(live) = live
        .as_ref()
        .filter(|live| !state.rules.counters_needed(live.solid))
    {
        let final_day = is_cacheable(date, today, "inverter", solid, None, end);
        return Ok((live_day(date, live), samples, final_day));
    }
    let cloud = if state.rules.counters_enabled() {
        cloud_day(state, date, start, end).await?
    } else {
        DayTotals::no_data(date, "cloud")
    };
    match live {
        Some(live) if live.usable(&state.rules, cloud.has_data()) => {
            let final_day = is_cacheable(date, today, "inverter", solid, None, end);
            Ok((live_day(date, &live), samples, final_day))
        }
        _ => {
            let newest = db::newest_reading_ts_in_range_from(
                &state.pool,
                &state.counter_source(),
                Metric::LoadDayWh,
                start,
                end,
            )
            .await?;
            let final_day = is_cacheable(date, today, "cloud", solid, newest, end);
            Ok((cloud, samples, final_day))
        }
    }
}

/// A day's totals from the live samples.
fn live_day(date: NaiveDate, live: &LiveTotals) -> DayTotals {
    DayTotals {
        date: date.to_string(),
        source: "inverter".into(),
        pv_kwh: Some(live.pv_kwh),
        load_kwh: Some(live.load_kwh),
        import_kwh: Some(live.import_kwh),
        export_kwh: Some(live.export_kwh),
        battery_charge_kwh: Some(live.battery_charge_kwh),
        battery_discharge_kwh: Some(live.battery_discharge_kwh),
        // Needs a pv-to-battery split we do not have; the cloud's value
        // would not match the live numbers, so leave it out.
        self_consumed_kwh: None,
        split: SplitSums::default(),
        home: HomeMix::default(),
    }
}

/// A day's totals from the iSolarCloud counters.
async fn cloud_day(
    state: &AppState,
    date: NaiveDate,
    start: i64,
    end: i64,
) -> Result<DayTotals, ApiError> {
    let load_kwh = day_max_kwh(state, Metric::LoadDayWh, start, end).await?;
    let import_kwh = day_max_kwh(state, Metric::GridImportDayWh, start, end).await?;
    let export_kwh = day_max_kwh(state, Metric::GridExportDayWh, start, end).await?;
    let pv_kwh = day_max_kwh(state, Metric::PvDayWh, start, end).await?;
    // A day where the cloud sent load readings but every counter stayed at 0
    // is a gap, not a day of no energy.
    if load_kwh.is_some()
        && !cloud_counters_rose([import_kwh, export_kwh, load_kwh])
        && !pv_kwh.is_some_and(|kwh| kwh > 0.0)
    {
        return Ok(DayTotals::no_data(date, "cloud"));
    }
    Ok(DayTotals {
        date: date.to_string(),
        source: "cloud".into(),
        pv_kwh,
        load_kwh,
        import_kwh,
        export_kwh,
        battery_charge_kwh: day_max_kwh(state, Metric::BatteryChargeDayWh, start, end).await?,
        battery_discharge_kwh: day_max_kwh(state, Metric::BatteryDischargeDayWh, start, end)
            .await?,
        self_consumed_kwh: day_max_kwh(state, Metric::SelfConsumedDayWh, start, end).await?,
        split: SplitSums::default(),
        home: HomeMix::default(),
    })
}

impl DayTotals {
    /// A day with no figures, as from `source`.
    fn no_data(date: NaiveDate, source: &'static str) -> Self {
        DayTotals {
            date: date.to_string(),
            source: source.into(),
            pv_kwh: None,
            load_kwh: None,
            import_kwh: None,
            export_kwh: None,
            battery_charge_kwh: None,
            battery_discharge_kwh: None,
            self_consumed_kwh: None,
            split: SplitSums::default(),
            home: HomeMix::default(),
        }
    }

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

/// Energy added up over any number of days, in kWh. A figure is `None` when
/// none of the days had it.
#[derive(Debug, Default, Serialize, PartialEq)]
struct EnergySums {
    /// Days with any data.
    days: u32,
    pv_kwh: Option<f64>,
    load_kwh: Option<f64>,
    import_kwh: Option<f64>,
    export_kwh: Option<f64>,
    battery_charge_kwh: Option<f64>,
    battery_discharge_kwh: Option<f64>,
    #[serde(flatten)]
    split: SplitSums,
    #[serde(flatten)]
    home: HomeMix,
}

impl EnergySums {
    fn add_day(&mut self, day: &DayTotals) {
        fn add(sum: &mut Option<f64>, v: Option<f64>) {
            if let Some(v) = v {
                *sum = Some(sum.unwrap_or(0.0) + v);
            }
        }
        if day.has_data() {
            self.days += 1;
        }
        add(&mut self.pv_kwh, day.pv_kwh);
        add(&mut self.load_kwh, day.load_kwh);
        add(&mut self.import_kwh, day.import_kwh);
        add(&mut self.export_kwh, day.export_kwh);
        add(&mut self.battery_charge_kwh, day.battery_charge_kwh);
        add(&mut self.battery_discharge_kwh, day.battery_discharge_kwh);
        self.split.merge(&day.split);
        add(&mut self.home.solar_kwh, day.home.solar_kwh);
        add(&mut self.home.battery_kwh, day.home.battery_kwh);
        add(&mut self.home.grid_kwh, day.home.grid_kwh);
        add(&mut self.home.grid_free_kwh, day.home.grid_free_kwh);
    }
}

#[derive(Debug, Serialize, PartialEq)]
struct EnergyBucket {
    /// "2026-09" for a month, "2026" for a year.
    period: String,
    #[serde(flatten)]
    sums: EnergySums,
}

/// Adds up days (oldest first) into one bucket per period that has data.
fn bucket_energy(group: Group, days: &[(NaiveDate, DayTotals)]) -> Vec<EnergyBucket> {
    let mut buckets: Vec<EnergyBucket> = Vec::new();
    for (date, day) in days.iter().filter(|(_, d)| d.has_data()) {
        let period = group.period_of(*date);
        if buckets.last().is_none_or(|b| b.period != period) {
            buckets.push(EnergyBucket {
                period,
                sums: EnergySums::default(),
            });
        }
        if let Some(bucket) = buckets.last_mut() {
            bucket.sums.add_day(day);
        }
    }
    buckets
}

/// The home load of one day by source (see `home_mix`), from the same data
/// the day's totals came from: the live samples, or else the cloud counters.
async fn day_home_mix(
    state: &AppState,
    data: &crate::admin_data::AdminData,
    date: NaiveDate,
    totals: &DayTotals,
    live: &[Sample],
) -> Result<HomeMix, ApiError> {
    let tariff = data.tariff.as_ref().map(|schedule| schedule.for_date(date));
    let steps = if totals.source == "inverter" {
        home_mix::live_steps(live)
    } else if totals.load_kwh.is_some() {
        let (start, end) = day_bounds_or_error(date, state.tz)?;
        let read = async |metric| -> Result<Vec<(i64, f64)>, ApiError> {
            let rows = db::readings_in_range_from(
                &state.pool,
                &state.counter_source(),
                metric,
                start,
                end,
            )
            .await?;
            Ok(undo_counter_lag(
                start,
                &rows,
                counter_lag_secs(&state.rules),
            ))
        };
        home_mix::cloud_steps(
            &read(Metric::PvDayWh).await?,
            &read(Metric::LoadDayWh).await?,
            &read(Metric::BatteryDischargeDayWh).await?,
        )
    } else {
        Vec::new()
    };
    Ok(home_mix::home_mix(&steps, tariff, state.tz))
}

/// The metrics `day_totals` reads from the cloud.
const DAILY_METRICS: [Metric; 7] = [
    Metric::PvDayWh,
    Metric::LoadDayWh,
    Metric::GridImportDayWh,
    Metric::GridExportDayWh,
    Metric::BatteryChargeDayWh,
    Metric::BatteryDischargeDayWh,
    Metric::SelfConsumedDayWh,
];

/// One local day's energy in kWh, for the Home Assistant feed. A field is
/// `None` when that day has no data for it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DayEnergy {
    pub pv_kwh: Option<f64>,
    pub load_kwh: Option<f64>,
    pub import_kwh: Option<f64>,
    pub export_kwh: Option<f64>,
    pub battery_charge_kwh: Option<f64>,
    pub battery_discharge_kwh: Option<f64>,
}

/// The same totals `/api/daily` (and so the Today page) shows for `date`:
/// `daily_days` is the one function behind both, so the two can never
/// disagree (and a finished day comes from the same per-day cache).
pub async fn day_energy(state: &AppState, date: NaiveDate) -> anyhow::Result<DayEnergy> {
    let live_range = samples::ts_range(&state.pool, &state.rules, Role::Daily).await?;
    let days = daily_days(state, &[date], live_range, db::now())
        .await
        .map_err(|err| anyhow::anyhow!("could not work out the day's totals: {}", err.message()))?;
    let Some((_, totals)) = days.into_iter().next() else {
        return Ok(DayEnergy::default());
    };
    Ok(DayEnergy {
        pv_kwh: totals.pv_kwh,
        load_kwh: totals.load_kwh,
        import_kwh: totals.import_kwh,
        export_kwh: totals.export_kwh,
        battery_charge_kwh: totals.battery_charge_kwh,
        battery_discharge_kwh: totals.battery_discharge_kwh,
    })
}

/// The `daily_cache` kind for [`daily_days`].
const DAILY_KIND: &str = "daily";

/// Bump when a day's figures in `/api/daily` would change for the same inputs
/// (the totals, the tariff split or the home mix).
const DAILY_VERSION: u32 = 1;

/// The full `/api/daily` figures of each of `dates` (oldest first). A finished
/// day with all its data (see [`is_cacheable`]) comes from the `daily_cache`
/// table when it was stored under the key it would get now (the version, the
/// time zone, the source rules, the plan for that date and the prices its
/// grid split used); the others read their samples, and the finished ones
/// are stored. Today and later are never stored.
async fn daily_days(
    state: &AppState,
    dates: &[NaiveDate],
    live_range: Option<(i64, i64)>,
    now: i64,
) -> Result<Vec<(NaiveDate, DayTotals)>, ApiError> {
    let data = state.admin.get();
    let today = today(state.tz);
    let spike_cents = data.price_spike_cents;
    let mut keys = Vec::new();
    for &date in dates.iter().filter(|d| **d < today) {
        let tariff = data.tariff.as_ref().map(|s| s.for_date(date));
        let mut key = format!(
            "{DAILY_VERSION}|{}|{:?}|spike={}",
            state.tz,
            state.rules,
            spike_cents.to_bits()
        );
        match tariff {
            Some(tariff) => {
                key.push_str(&format!("|{}", cache_key(state.tz, tariff)));
                if let Some(source) = tariff.dynamic_source() {
                    let (start, end) = day_bounds_or_error(date, state.tz)?;
                    let fingerprint =
                        amber_store::day_prices_fingerprint(&state.pool, source, start, end, false)
                            .await?;
                    key.push_str(&format!("|prices:{fingerprint}"));
                }
            }
            None => key.push_str("|no-plan"),
        }
        keys.push((date, key));
    }
    let mut stored: HashMap<NaiveDate, DayTotals> = day_cache::load_with_data(
        &state.pool,
        state.plant_id,
        state.rules.uses_ha(),
        state.tz,
        DAILY_KIND,
        &keys,
    )
    .await?;

    let mut out = Vec::with_capacity(dates.len());
    let mut to_store = Vec::new();
    for &date in dates {
        if let Some(day) = stored.remove(&date) {
            out.push((date, day));
            continue;
        }
        let (mut totals, live, mut final_day) =
            day_totals_flagged(state, date, live_range, now).await?;
        if let Some(schedule) = &data.tariff
            && let Some((split, cacheable)) =
                day_grid_split_checked(state, schedule, date, &live, now).await?
        {
            totals.split.add(&split);
            final_day &= cacheable;
        }
        totals.home = day_home_mix(state, &data, date, &totals, &live).await?;
        // A finished day with gaps is stored too, keyed on its readings.
        if let Some((_, key)) = keys.iter().find(|(d, _)| *d == date) {
            to_store.push((date, key.clone(), final_day, out.len()));
        }
        out.push((date, totals));
    }
    let store_as: Vec<(NaiveDate, String, bool)> = to_store
        .iter()
        .map(|(date, key, complete, _)| (*date, key.clone(), *complete))
        .collect();
    let store_as = day_cache::store_keys(
        &state.pool,
        state.plant_id,
        state.rules.uses_ha(),
        state.tz,
        &store_as,
    )
    .await?;
    let rows: Vec<(NaiveDate, String, &DayTotals)> = store_as
        .into_iter()
        .zip(&to_store)
        .map(|((date, key), (_, _, _, i))| (date, key, &out[*i].1))
        .collect();
    day_cache::save(&state.pool, DAILY_KIND, &rows).await?;
    Ok(out)
}

pub async fn daily(State(state): State<AppState>, Query(params): Query<DailyParams>) -> ApiResult {
    let group = Group::parse(params.group.as_deref())?;
    // Grouped rows can cover the whole history. The days are still read one by one,
    // but only the answer (a few hundred rows at most) is sent.
    let max_days = match group {
        Group::Day => MAX_DAILY_DAYS,
        Group::Month | Group::Year => MAX_GROUPED_COSTS_DAYS,
    };
    let (mut from, mut count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        max_days,
        DEFAULT_DAILY_DAYS,
    )?;

    let live_range = samples::ts_range(&state.pool, &state.rules, Role::Daily).await?;
    let now = db::now();
    if group != Group::Day {
        // Days before the first reading have nothing in them; do not read them.
        let cloud_first =
            db::first_reading_ts_from(&state.pool, &state.counter_source(), &DAILY_METRICS).await?;
        let first = [cloud_first, live_range.map(|(first, _)| first)]
            .into_iter()
            .flatten()
            .min();
        let to = from + Days::new(count - 1);
        let first_day = first
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
            .map(|t| t.with_timezone(&state.tz).date_naive());
        match first_day {
            Some(day) if day > from => {
                // Past `to` means no day in range can have data.
                count = if day > to {
                    0
                } else {
                    (to - day).num_days() as u64 + 1
                };
                from = day;
            }
            None => count = 0,
            _ => {}
        }
    }
    let dates: Vec<NaiveDate> = from.iter_days().take(count as usize).collect();
    let days = daily_days(&state, &dates, live_range, now).await?;
    if group == Group::Day {
        let days: Vec<_> = days.into_iter().map(|(_, d)| d).collect();
        return Ok(Json(json!({ "days": days })));
    }
    let mut totals = EnergySums::default();
    for (_, day) in &days {
        totals.add_day(day);
    }
    Ok(Json(json!({
        "group": if group == Group::Month { "month" } else { "year" },
        "totals": totals,
        "buckets": bucket_energy(group, &days),
    })))
}

#[cfg(test)]
mod tests {
    use axum_extra::extract::cookie::Key;

    use super::*;

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    #[test]
    fn grid_pull_counts_import_only() {
        let sample = |grid_w| Sample {
            ts: 100,
            pv_w: None,
            load_w: None,
            grid_w,
            battery_w: None,
            soc_pct: None,
        };
        // Positive grid_w is importing: 13.8 kW pulled, and the peak is passed through.
        let json = grid_pull_json(
            Some(&sample(Some(13_800.4))),
            Some((90, 14_600.0)),
            12.0,
            15.0,
        );
        assert_eq!(
            json,
            json!({
                "now_w": 13800, "peak_today_w": 14600, "peak_today_at": 90,
                "warn_w": 12000, "limit_w": 15000, "volts": null,
            })
        );
        // Negative is exporting: nothing is pulled.
        assert_eq!(
            grid_pull_json(Some(&sample(Some(-2_500.0))), None, 12.0, 15.0)["now_w"],
            0
        );
        // No fresh sample, or an unavailable reading: unknown, not 0. No peak: null.
        let none = grid_pull_json(None, None, 12.5, 15.0);
        assert!(none["now_w"].is_null() && none["peak_today_w"].is_null());
        assert_eq!(none["warn_w"], 12500);
        assert!(grid_pull_json(Some(&sample(None)), None, 12.0, 15.0)["now_w"].is_null());
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
    fn big_step_after_quiet_spell_goes_to_the_last_reading() {
        // Still since 10:40, then +1400 Wh at 11:05, readings every 5 minutes.
        let t = |h: i64, m: i64| h * 3600 + m * 60;
        let mut counter = vec![(t(10, 30), 900.0), (t(10, 35), 900.0)];
        counter.extend((0..5).map(|i| (t(10, 40) + i * 300, 1000.0)));
        counter.push((t(11, 5), 2400.0));
        let steps = spread_steps(&counter);
        // steps[0] is the move to 1000 at 10:40.
        assert_eq!(
            steps[1..],
            [
                Spread {
                    start: t(10, 40),
                    end: t(11, 0),
                    wh: 100.0
                },
                Spread {
                    start: t(11, 0),
                    end: t(11, 5),
                    wh: 1300.0
                },
            ]
        );
        // The energy still adds up to the counter's rise.
        let total: f64 = steps.iter().map(|s| s.wh).sum();
        assert_eq!(total, 1500.0);
        // Power rows: 300 W up to 11:00, then 15.6 kW for the last row.
        let power = derive_power(&counter);
        assert_eq!(power.last(), Some(&(t(11, 5), 1300.0 * 3600.0 / 300.0)));
        assert_eq!(power[power.len() - 2], (t(11, 0), 300.0));
        // A big step with no quiet spell (the counter moved at the reading
        // before) is not split.
        let counter = [(0, 0.0), (300, 100.0), (600, 600.0)];
        assert_eq!(
            spread_steps(&counter)[1..],
            [Spread {
                start: 300,
                end: 600,
                wh: 500.0
            }]
        );
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
        let readings: Vec<_> = rows
            .iter()
            .map(|&(ts, value)| (Metric::PvDayWh, ts, value))
            .collect();
        db::seed_metrics(&pool, 7, &readings).await;

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
        let rules = SourceRules::default();
        let (start, end) = (0, 86_400);
        let later = 1_000_000; // a later day: the day is finished
        // A finished day is solid from 98% (84,672 s).
        assert_eq!(live_coverage(84_672, start, end, later, &rules), Solid);
        assert_eq!(live_coverage(84_671, start, end, later, &rules), Partial);
        assert_eq!(live_coverage(77_760, start, end, later, &rules), Partial); // exactly 90%
        assert_eq!(live_coverage(77_759, start, end, later, &rules), Thin);
        // Today at 06:00: 90% of the 6 hours so far is enough, and solid.
        let now = 6 * 3600;
        assert_eq!(live_coverage(19_440, start, end, now, &rules), Solid);
        assert_eq!(live_coverage(19_439, start, end, now, &rules), Thin);
        // A future day has nothing elapsed, so it never uses live.
        assert_eq!(live_coverage(0, start, end, -1, &rules), Thin);
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
    fn all_zero_cloud_counters_are_not_data() {
        assert!(!cloud_counters_rose([None, None, None]));
        assert!(!cloud_counters_rose([Some(0.0), Some(0.0), Some(0.0)]));
        assert!(cloud_counters_rose([Some(0.0), Some(0.0), Some(4.2)]));
        assert!(cloud_counters_rose([Some(1.0), None, None]));
    }

    #[test]
    fn a_partial_live_day_gives_way_to_the_cloud_only_when_it_has_data() {
        // 95% live with cloud present: the cloud wins.
        assert!(!totals(false).usable(&SourceRules::default(), true));
        // 95% live and no cloud: keep the live numbers, do not blank the day.
        assert!(totals(false).usable(&SourceRules::default(), false));
        // 99% live: live wins either way.
        assert!(totals(true).usable(&SourceRules::default(), true));
        assert!(totals(true).usable(&SourceRules::default(), false));
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
        let partial =
            live_totals(&samples(82_080), start, end, now, &SourceRules::default()).unwrap(); // 95%
        assert!(!partial.solid);
        let solid =
            live_totals(&samples(85_500), start, end, now, &SourceRules::default()).unwrap(); // 99%
        assert!(solid.solid);
        assert!(live_totals(&samples(70_000), start, end, now, &SourceRules::default()).is_none());
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
    async fn inverter_adds_labels_and_hides_old_settings() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let settings = modbus::Settings {
            ts: db::now() - 5,
            ems_mode: Some(2),
            forced_command: Some(0xAA),
            forced_power_w: Some(5000),
            max_soc_pct: Some(100.0),
            min_soc_pct: Some(5.0),
            export_limit_w: Some(5000),
            export_limit_enabled: Some(true),
            backup_reserve_pct: Some(10),
            max_charge_w: None,
            max_discharge_w: Some(8000),
            running_state: Some(0),
        };
        state.latest_settings().set(settings.clone());
        let Json(json) = inverter(State(state.clone())).await.unwrap();
        assert_eq!(json["settings"]["ems_mode"], 2);
        assert_eq!(
            json["settings"]["ems_mode_label"],
            "Forced (compulsory) mode"
        );
        assert_eq!(json["settings"]["forced_command_label"], "Forced charge");
        assert_eq!(json["settings"]["running_state_label"], "Running");
        assert_eq!(json["settings"]["forced_power_w"], 5000);
        assert!(json["settings"]["max_charge_w"].is_null());

        state.latest_settings().set(modbus::Settings {
            ts: db::now() - 600,
            ..settings
        });
        let Json(json) = inverter(State(state)).await.unwrap();
        assert!(json["settings"].is_null());
    }

    #[tokio::test]
    async fn inverter_history_lists_changes_newest_first_with_labels() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool.clone(), 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let params = |from: Option<&str>, to: Option<&str>| InverterHistoryParams {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        };

        // Nothing saved yet.
        let Json(json) = inverter_history(State(state.clone()), Query(params(None, None)))
            .await
            .unwrap();
        assert_eq!(json, json!({ "changes": [], "previous": null }));

        let now = db::now();
        let row = |ts, ems_mode, running_state| modbus::Settings {
            ts,
            ems_mode: Some(ems_mode),
            forced_command: Some(0xAA),
            forced_power_w: Some(5000),
            max_soc_pct: None,
            min_soc_pct: None,
            export_limit_w: None,
            export_limit_enabled: None,
            backup_reserve_pct: Some(10),
            max_charge_w: None,
            max_discharge_w: None,
            running_state: Some(running_state),
        };
        db::insert_inverter_settings(&pool, &row(now - 3600, 0, 0))
            .await
            .unwrap();
        db::insert_inverter_settings(&pool, &row(now - 60, 2, 0x8200))
            .await
            .unwrap();
        // Older than the default 7 days.
        db::insert_inverter_settings(&pool, &row(now - 8 * 24 * 3600, 0, 0))
            .await
            .unwrap();

        let Json(json) = inverter_history(State(state.clone()), Query(params(None, None)))
            .await
            .unwrap();
        let changes = json["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["ts"], now - 60);
        assert_eq!(changes[0]["ems_mode_label"], "Forced (compulsory) mode");
        assert_eq!(changes[0]["forced_command_label"], "Forced charge");
        assert_eq!(changes[0]["running_state_label"], "Dispatch running");
        assert_eq!(changes[1]["ems_mode_label"], "Self-consumption");
        assert!(changes[1]["max_soc_pct"].is_null());
        // The row from 8 days ago is the setting in force when the range starts.
        assert_eq!(json["previous"]["ts"], now - 8 * 24 * 3600);
        assert_eq!(json["previous"]["ems_mode_label"], "Self-consumption");

        // An explicit range: `to` is exclusive.
        let from = (now - 3600).to_string();
        let to = (now - 60).to_string();
        let Json(json) = inverter_history(State(state), Query(params(Some(&from), Some(&to))))
            .await
            .unwrap();
        assert_eq!(json["changes"].as_array().unwrap().len(), 1);
        assert_eq!(json["previous"]["ts"], now - 8 * 24 * 3600);
    }

    #[tokio::test]
    async fn inverter_history_rejects_bad_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let call = |from: &str, to: &str| {
            inverter_history(
                State(state.clone()),
                Query(InverterHistoryParams {
                    from: Some(from.to_string()),
                    to: Some(to.to_string()),
                }),
            )
        };
        assert!(call("abc", "100").await.is_err());
        assert!(call("200", "100").await.is_err());
        let too_long = (401 * 24 * 3600).to_string();
        assert!(call("0", &too_long).await.is_err());
        let ok = (400 * 24 * 3600).to_string();
        assert!(call("0", &ok).await.is_ok());
    }

    #[tokio::test]
    async fn daily_prefers_live_samples_when_they_cover_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        // Cloud counter for both days: 5 kWh.
        let state_day = |date: &str| {
            let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
            day_bounds(date, ADELAIDE).unwrap()
        };
        let (start, end) = state_day("2026-10-19");
        let (start2, _) = state_day("2026-10-20");
        let cloud: Vec<_> = [start + 3600, start2 + 3600]
            .iter()
            .map(|&ts| (Metric::GridImportDayWh, ts, 5000.0))
            .collect();
        db::seed_metrics(&pool, 7, &cloud).await;

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

    /// A finished day with cloud data and live samples covering `covered_pct`
    /// percent of it. Returns the day's source under the given rules.
    async fn source_with_rules(covered_pct: i64, rules: SourceRules) -> String {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let day = NaiveDate::parse_from_str("2026-10-19", "%Y-%m-%d").unwrap();
        let (start, end) = day_bounds(day, ADELAIDE).unwrap();
        db::seed_metrics(&pool, 7, &[(Metric::GridImportDayWh, start + 3600, 5000.0)]).await;
        let covered_end = start + (end - start) * covered_pct / 100;
        // One sample a minute, written in one statement: inserting them one by
        // one made this the slowest test in the suite.
        sqlx::query(
            "INSERT INTO live_sample (ts, pv_w, load_w, grid_w, battery_w, soc_pct)
             WITH RECURSIVE t(ts) AS (SELECT ?1 UNION ALL SELECT ts + 60 FROM t WHERE ts + 60 < ?2)
             SELECT ts, 0.0, 1000.0, 1000.0, 0.0, 50.0 FROM t",
        )
        .bind(start)
        .bind(covered_end)
        .execute(&pool)
        .await
        .unwrap();
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate())
            .unwrap()
            .with_source_rules(rules);
        let range = db::live_ts_range(&state.pool).await.unwrap();
        day_totals(&state, day, range, end + 3600)
            .await
            .unwrap()
            .source
            .into_owned()
    }

    #[tokio::test]
    async fn source_rules_change_which_source_wins() {
        let rules = |ids: &[&str], finished, partial| {
            SourceRules::from_config(
                &crate::config::Sources {
                    daily_energy: Some(ids.iter().map(|id| id.to_string()).collect()),
                    finished_day_coverage: finished,
                    partial_day_coverage: partial,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
        };
        let live_first = ["sungrow-modbus", "isolarcloud"];
        let cloud_first = ["isolarcloud", "sungrow-modbus"];
        // Defaults: a fully covered day is live; at 95% it is only partial,
        // so the cloud (which has data) wins.
        assert_eq!(
            source_with_rules(100, SourceRules::default()).await,
            "inverter"
        );
        assert_eq!(source_with_rules(95, SourceRules::default()).await, "cloud");
        // A lower finished-day bar makes 95% solid.
        assert_eq!(
            source_with_rules(95, rules(&live_first, Some(0.94), None)).await,
            "inverter"
        );
        // A higher partial bar makes 95% thin even when it would be solid.
        assert_eq!(
            source_with_rules(95, rules(&live_first, Some(1.0), Some(0.97))).await,
            "cloud"
        );
        // Cloud first: the cloud wins even over a fully covered live day.
        assert_eq!(
            source_with_rules(100, rules(&cloud_first, None, None)).await,
            "cloud"
        );
        // Live only / cloud only.
        assert_eq!(
            source_with_rules(100, rules(&["sungrow-modbus"], None, None)).await,
            "inverter"
        );
        assert_eq!(
            source_with_rules(100, rules(&["isolarcloud"], None, None)).await,
            "cloud"
        );
        // Live only, but too thin: there is no other source, so no figures.
        let empty = source_with_rules(10, rules(&["sungrow-modbus"], None, None)).await;
        assert_eq!(empty, "cloud");
    }

    fn totals_of(date: &str, pv: Option<f64>, import: Option<f64>) -> (NaiveDate, DayTotals) {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        let day = DayTotals {
            date: date.to_string(),
            source: "cloud".into(),
            pv_kwh: pv,
            load_kwh: None,
            import_kwh: import,
            export_kwh: None,
            battery_charge_kwh: None,
            battery_discharge_kwh: None,
            self_consumed_kwh: None,
            split: SplitSums::default(),
            home: HomeMix::default(),
        };
        (date, day)
    }

    #[test]
    fn tariff_bands_are_summed_into_buckets() {
        let split = |free: f64, peak: f64| GridSplit {
            import_free_kwh: free,
            import_peak_kwh: peak,
            export_super_kwh: 1.0,
            ..GridSplit::default()
        };
        let mut a = totals_of("2026-01-01", Some(1.0), Some(1.0));
        a.1.split.add(&split(1.0, 0.5));
        let mut b = totals_of("2026-01-02", Some(1.0), Some(1.0));
        b.1.split.add(&split(2.0, 0.0));
        // A day with data but no intervals leaves the sums alone.
        let c = totals_of("2026-01-03", Some(1.0), None);
        let d = totals_of("2026-02-01", Some(1.0), None);
        let months = bucket_energy(Group::Month, &[a, b, c, d]);
        assert_eq!(months[0].sums.split.import_free_kwh, Some(3.0));
        assert_eq!(months[0].sums.split.import_peak_kwh, Some(0.5));
        assert_eq!(months[0].sums.split.export_super_kwh, Some(2.0));
        assert_eq!(months[0].sums.split.import_shoulder_kwh, Some(0.0));
        // February had no interval data at all.
        assert_eq!(months[1].sums.split, SplitSums::default());
    }

    #[test]
    fn energy_is_summed_into_months_and_years() {
        let days = [
            totals_of("2025-12-31", Some(10.0), Some(1.0)),
            totals_of("2026-01-01", Some(20.0), None),
            totals_of("2026-01-02", Some(5.5), Some(2.0)),
            // No data: no day counted, and February gets no bucket.
            totals_of("2026-02-01", None, None),
            totals_of("2026-03-05", Some(1.0), None),
        ];
        let months = bucket_energy(Group::Month, &days);
        let summary: Vec<_> = months
            .iter()
            .map(|b| {
                (
                    b.period.as_str(),
                    b.sums.days,
                    b.sums.pv_kwh,
                    b.sums.import_kwh,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("2025-12", 1, Some(10.0), Some(1.0)),
                ("2026-01", 2, Some(25.5), Some(2.0)),
                ("2026-03", 1, Some(1.0), None),
            ]
        );
        let years = bucket_energy(Group::Year, &days);
        let summary: Vec<_> = years
            .iter()
            .map(|b| (b.period.as_str(), b.sums.days, b.sums.pv_kwh))
            .collect();
        assert_eq!(summary, [("2025", 1, Some(10.0)), ("2026", 3, Some(26.5))]);
        assert!(bucket_energy(Group::Year, &[]).is_empty());
    }

    #[tokio::test]
    async fn daily_splits_grid_energy_by_tariff_band() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let globird: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 150.0
            default_import_cents = 40.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "16:00", to = "23:00", cents = 60.0 },
            ]
            default_export_cents = 0.0
            export = [
              { from = "18:00", to = "21:00", cents = 10.0 },
              { from = "16:00", to = "23:00", cents = 2.0 },
            ]
            "#,
        )
        .unwrap();
        let schedule = crate::cost::TariffSchedule::from_config(&globird.tariff, &[]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let start = day_bounds(date("2026-10-20"), ADELAIDE).unwrap().0;
        let at = |hour: i64, minute: i64| start + hour * 3600 + minute * 60;
        // Import: 1 kWh in the free window, 0.5 kWh in the peak.
        // Export: 2 kWh in the super window, 1 kWh in the rest of the peak.
        let counter = |metric: Metric, rows: &[(i64, i64, f64)]| -> Vec<(Metric, i64, f64)> {
            rows.iter()
                .map(|&(h, m, wh)| (metric, at(h, m), wh))
                .collect()
        };
        let mut rows = counter(
            Metric::GridImportDayWh,
            &[
                (12, 0, 0.0),
                (12, 5, 1000.0),
                (17, 0, 1000.0),
                (17, 5, 1500.0),
            ],
        );
        rows.extend(counter(
            Metric::GridExportDayWh,
            &[
                (19, 0, 0.0),
                (19, 5, 2000.0),
                (22, 0, 2000.0),
                (22, 5, 3000.0),
            ],
        ));
        db::seed_metrics(&pool, 7, &rows).await;
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let with_tariff = state.clone().with_tariff(Some(schedule));
        let ask = |group: Option<&str>| DailyParams {
            from: Some("2026-10-20".to_string()),
            to: Some("2026-10-21".to_string()),
            group: group.map(str::to_string),
        };

        let Json(json) = daily(State(with_tariff.clone()), Query(ask(None)))
            .await
            .unwrap();
        let day = &json["days"][0];
        let near = |v: &serde_json::Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-9;
        assert!(near(&day["import_free_kwh"], 1.0));
        assert!(near(&day["import_shoulder_kwh"], 0.0));
        assert!(near(&day["import_peak_kwh"], 0.5));
        assert!(near(&day["export_super_kwh"], 2.0));
        assert!(near(&day["export_peak_kwh"], 1.0));
        assert!(near(&day["export_other_kwh"], 0.0));
        assert!(near(&day["export_tier_kwh"], 0.0));
        // The existing fields are unchanged, and the parts add up to them.
        assert_eq!(day["import_kwh"], 1.5);
        assert_eq!(day["export_kwh"], 3.0);
        // The next day has no data: null, not zero.
        assert!(json["days"][1]["import_free_kwh"].is_null());
        assert!(json["days"][1]["export_other_kwh"].is_null());

        let Json(json) = daily(State(with_tariff), Query(ask(Some("month"))))
            .await
            .unwrap();
        assert!(near(&json["totals"]["import_free_kwh"], 1.0));
        assert!(near(&json["buckets"][0]["export_super_kwh"], 2.0));

        // Without a tariff the fields are there but null.
        let Json(json) = daily(State(state), Query(ask(None))).await.unwrap();
        assert!(json["days"][0]["import_free_kwh"].is_null());
        assert_eq!(json["days"][0]["import_kwh"], 1.5);
    }

    #[tokio::test]
    async fn daily_stored_days_answer_the_same_as_worked_out_ones() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let plan: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 150.0
            default_import_cents = 40.0
            import = [{ from = "11:00", to = "14:00", cents = 0.0 }]
            default_export_cents = 5.0
            "#,
        )
        .unwrap();
        let schedule = crate::cost::TariffSchedule::from_config(&plan.tariff, &[]).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let start = day_bounds(date("2026-10-20"), ADELAIDE).unwrap().0;
        let at = |hour: i64, minute: i64| start + hour * 3600 + minute * 60;
        let rows = [
            (Metric::PvDayWh, at(12, 0), 5000.0),
            (Metric::GridImportDayWh, at(12, 0), 0.0),
            (Metric::GridImportDayWh, at(12, 5), 1000.0),
            (Metric::GridImportDayWh, at(17, 0), 1000.0),
            (Metric::GridImportDayWh, at(17, 5), 1500.0),
            // The last reading of the day is what makes a cloud day final.
            (Metric::LoadDayWh, at(23, 55), 9000.0),
        ];
        db::seed_metrics(&pool, 7, &rows).await;
        let hash = super::super::hash_password("x").unwrap();
        let plain = AppState::new(pool.clone(), 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let state = plain.clone().with_tariff(Some(schedule));
        let ask = |group: Option<&str>| DailyParams {
            from: Some("2026-10-19".to_string()),
            to: Some("2026-10-21".to_string()),
            group: group.map(str::to_string),
        };
        let stored = || async {
            db::daily_cache_in_range(&pool, DAILY_KIND, "2026-10-01", "2026-10-31")
                .await
                .unwrap()
                .len()
        };
        let clear = || async {
            sqlx::query("DELETE FROM daily_cache")
                .execute(&pool)
                .await
                .unwrap();
        };

        for group in [None, Some("month")] {
            clear().await;
            let Json(worked_out) = daily(State(state.clone()), Query(ask(group)))
                .await
                .unwrap();
            // The finished days are stored (a day before any reading, or one
            // a grouped request skips, may not be), and a second request
            // stores no more.
            let after_first = stored().await;
            assert!(after_first >= 2);
            let Json(from_store) = daily(State(state.clone()), Query(ask(group)))
                .await
                .unwrap();
            assert_eq!(stored().await, after_first);
            assert_eq!(worked_out.to_string(), from_store.to_string());
        }

        // A stored day is not used for another plan: without one the split is null.
        let Json(with_plan) = daily(State(state.clone()), Query(ask(None))).await.unwrap();
        assert_eq!(with_plan["days"][1]["import_free_kwh"], 1.0);
        let Json(no_plan) = daily(State(plain), Query(ask(None))).await.unwrap();
        assert!(no_plan["days"][1]["import_free_kwh"].is_null());
        assert_eq!(no_plan["days"][1]["import_kwh"], 1.5);
    }

    #[tokio::test]
    async fn a_finished_day_with_gaps_is_stored_keyed_on_its_readings() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let start = day_bounds(date("2026-10-20"), ADELAIDE).unwrap().0;
        let today_date = today(ADELAIDE);
        let today_start = day_bounds(today_date, ADELAIDE).unwrap().0;
        // The cloud stopped at noon: no reading in the day's last 10 minutes,
        // so the day is not complete.
        let rows = [
            (Metric::PvDayWh, start + 12 * 3600, 5000.0),
            (Metric::LoadDayWh, start + 12 * 3600, 3000.0),
            // Today has readings too, and must never be stored.
            (Metric::LoadDayWh, today_start + 3600, 1000.0),
        ];
        db::seed_metrics(&pool, 7, &rows).await;
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool.clone(), 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let params = DailyParams {
            from: Some("2026-10-19".to_string()),
            to: Some(today_date.to_string()),
            group: None,
        };
        let stored = || async {
            db::daily_cache_in_range(&pool, DAILY_KIND, "2026-10-01", "2099-01-01")
                .await
                .unwrap()
        };

        let Json(worked_out) = daily(State(state.clone()), Query(params.clone()))
            .await
            .unwrap();
        let rows = stored().await;
        // The incomplete day is stored under its data key; today is not.
        assert!(rows.iter().all(|r| r.date != today_date.to_string()));
        let row = rows.iter().find(|r| r.date == "2026-10-20").unwrap();
        assert!(row.cache_key.contains("|data:"));
        let key_before = row.cache_key.clone();
        let Json(from_store) = daily(State(state.clone()), Query(params.clone()))
            .await
            .unwrap();
        assert_eq!(worked_out.to_string(), from_store.to_string());
        assert_eq!(stored().await, rows);

        // A backfilled reading changes the key, and the day is worked out again.
        db::seed_metrics(&pool, 7, &[(Metric::LoadDayWh, start + 13 * 3600, 4000.0)]).await;
        let Json(after) = daily(State(state), Query(params)).await.unwrap();
        let now_rows = stored().await;
        let row = now_rows.iter().find(|r| r.date == "2026-10-20").unwrap();
        assert_ne!(row.cache_key, key_before);
        assert_ne!(after.to_string(), from_store.to_string());
    }

    #[tokio::test]
    async fn daily_group_sums_days_and_skips_the_time_before_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        let day_start = |s: &str| {
            let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
            day_bounds(date, ADELAIDE).unwrap().0
        };
        let rows: Vec<_> = [
            ("2025-12-30", 4000.0),
            ("2026-01-02", 6000.0),
            ("2026-01-03", 1000.0),
        ]
        .iter()
        .map(|&(d, v)| (Metric::PvDayWh, day_start(d) + 3600, v))
        .collect();
        db::seed_metrics(&pool, 7, &rows).await;
        let hash = super::super::hash_password("x").unwrap();
        let state = AppState::new(pool, 7, ADELAIDE, &hash, Key::generate()).unwrap();
        let ask = |from: &str, to: &str, group: &str| DailyParams {
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            group: Some(group.to_string()),
        };

        // Twenty years back: the empty years are skipped, not read.
        let Json(json) = daily(
            State(state.clone()),
            Query(ask("2007-01-01", "2026-03-01", "year")),
        )
        .await
        .unwrap();
        assert_eq!(json["group"], "year");
        assert_eq!(json["totals"]["pv_kwh"], 11.0);
        assert_eq!(json["totals"]["days"], 3);
        let buckets = json["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0]["period"], "2025");
        assert_eq!(buckets[0]["pv_kwh"], 4.0);
        assert_eq!(buckets[1]["period"], "2026");
        assert_eq!(buckets[1]["days"], 2);
        assert_eq!(buckets[1]["pv_kwh"], 7.0);

        // A range wholly before the data is empty.
        let Json(json) = daily(
            State(state.clone()),
            Query(ask("2020-01-01", "2020-12-31", "month")),
        )
        .await
        .unwrap();
        assert_eq!(json["buckets"].as_array().unwrap().len(), 0);
        assert_eq!(json["totals"]["days"], 0);

        // Without a group the answer is as before, and still capped at 400 days.
        let plain = DailyParams {
            from: Some("2025-12-30".to_string()),
            to: Some("2025-12-31".to_string()),
            group: None,
        };
        let Json(json) = daily(State(state.clone()), Query(plain)).await.unwrap();
        assert_eq!(json["days"].as_array().unwrap().len(), 2);
        assert!(
            daily(
                State(state.clone()),
                Query(ask("2020-01-01", "2026-01-01", "day"))
            )
            .await
            .is_err()
        );
        assert!(
            daily(State(state), Query(ask("2025-12-30", "2025-12-31", "week")))
                .await
                .is_err()
        );
    }
}
