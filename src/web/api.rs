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
use super::costs::{Group, MAX_GROUPED_COSTS_DAYS, day_grid_split, round2};
use crate::cost::{GridSplit, TariffSchedule};
use crate::db::{self, LatestReading};
use crate::ev_cost;
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
    let data = state.admin.get();
    let Some(schedule) = &data.tariff else {
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

    let prices = state.admin.get().ev_prices;
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
/// charges are left out.
pub async fn ev_cars(State(state): State<AppState>) -> ApiResult {
    let data = state.admin.get();
    let prices = data.ev_prices;
    let petrol = &data.petrol;
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
                "name": car.name,
                "model": car.model,
                "label": label,
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
    /// "day" (the default), "month" or "year": sum the days into one row per period.
    group: Option<String>,
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
    /// Grid energy by tariff band; all `null` without a tariff or interval data.
    #[serde(flatten)]
    split: SplitSums,
}

/// Grid energy by tariff band in kWh (see `cost::grid_split`), summed over
/// any number of days. A figure is `None` when no day had interval data.
#[derive(Debug, Default, Serialize, PartialEq)]
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

/// The battery's discharge in kWh on each of `dates` that has a figure, the
/// same one `/api/daily` reports (the live samples when they cover the day,
/// else the cloud counter). Days without a figure are left out.
pub(super) async fn battery_discharge_by_date(
    state: &AppState,
    dates: &[NaiveDate],
) -> Result<std::collections::BTreeMap<NaiveDate, f64>, ApiError> {
    let live_range = db::live_ts_range(&state.pool).await?;
    let now = db::now();
    let mut found = std::collections::BTreeMap::new();
    for &date in dates {
        let (totals, _) = day_totals_with_live(state, date, live_range, now).await?;
        if let Some(kwh) = totals.battery_discharge_kwh {
            found.insert(date, kwh);
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
    Ok(day_totals_with_live(state, date, live_range, now).await?.0)
}

/// [`day_totals`], plus the live samples it loaded (empty when none), so the
/// tariff split can reuse them instead of querying again.
async fn day_totals_with_live(
    state: &AppState,
    date: NaiveDate,
    live_range: Option<(i64, i64)>,
    now: i64,
) -> Result<(DayTotals, Vec<Sample>), ApiError> {
    // Each day gets its own [start, end), so a 23 or 25 hour day is handled correctly.
    let (start, end) = day_bounds_or_error(date, state.tz)?;
    let samples = match live_range {
        Some((first, last)) if first < end && last >= start => {
            db::live_samples_raw(&state.pool, start, end).await?
        }
        _ => Vec::new(),
    };
    let live = live_totals(&samples, start, end, now);
    // A solid live day never needs the cloud; skip its queries.
    if let Some(live) = live.as_ref().filter(|live| live.solid) {
        return Ok((live_day(date, live), samples));
    }
    let cloud = cloud_day(state, date, start, end).await?;
    match live {
        Some(live) if live.usable(cloud.has_data()) => Ok((live_day(date, &live), samples)),
        _ => Ok((cloud, samples)),
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
        split: SplitSums::default(),
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
        split: SplitSums::default(),
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

/// The points `day_totals` reads from the cloud.
const DAILY_POINTS: [i64; 7] = [83022, 83118, 83102, 83072, 83322, 83323, 83097];

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

    let live_range = db::live_ts_range(&state.pool).await?;
    let now = db::now();
    if group != Group::Day {
        // Days before the first reading have nothing in them; do not read them.
        let cloud_first = db::first_reading_ts(&state.pool, state.plant_id, &DAILY_POINTS).await?;
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
    let data = state.admin.get();
    let mut days = Vec::new();
    for date in from.iter_days().take(count as usize) {
        let (mut totals, live) = day_totals_with_live(&state, date, live_range, now).await?;
        if let Some(schedule) = &data.tariff
            && let Some(split) = day_grid_split(&state, schedule, date, &live, now).await?
        {
            totals.split.add(&split);
        }
        days.push((date, totals));
    }
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

    fn totals_of(date: &str, pv: Option<f64>, import: Option<f64>) -> (NaiveDate, DayTotals) {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
        let day = DayTotals {
            date: date.to_string(),
            source: "cloud",
            pv_kwh: pv,
            load_kwh: None,
            import_kwh: import,
            export_kwh: None,
            battery_charge_kwh: None,
            battery_discharge_kwh: None,
            self_consumed_kwh: None,
            split: SplitSums::default(),
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
        for point_id in [83102, 83072, 83118] {
            let point = Point {
                point_id,
                code: None,
                name: None,
                unit: Some("Wh".to_string()),
            };
            db::upsert_point(&pool, &point).await.unwrap();
        }
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let start = day_bounds(date("2026-10-20"), ADELAIDE).unwrap().0;
        let at = |hour: i64, minute: i64| start + hour * 3600 + minute * 60;
        // Import: 1 kWh in the free window, 0.5 kWh in the peak.
        // Export: 2 kWh in the super window, 1 kWh in the rest of the peak.
        let counter = |point_id: i64, rows: &[(i64, i64, f64)]| -> Vec<Reading> {
            rows.iter()
                .map(|&(h, m, wh)| Reading {
                    point_id,
                    ts: at(h, m),
                    value: Some(wh),
                })
                .collect()
        };
        let mut rows = counter(
            83102,
            &[
                (12, 0, 0.0),
                (12, 5, 1000.0),
                (17, 0, 1000.0),
                (17, 5, 1500.0),
            ],
        );
        rows.extend(counter(
            83072,
            &[
                (19, 0, 0.0),
                (19, 5, 2000.0),
                (22, 0, 2000.0),
                (22, 5, 3000.0),
            ],
        ));
        db::upsert_readings(&pool, 7, &rows).await.unwrap();
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
    async fn daily_group_sums_days_and_skips_the_time_before_the_data() {
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
        let day_start = |s: &str| {
            let date = NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
            day_bounds(date, ADELAIDE).unwrap().0
        };
        let rows: Vec<Reading> = [
            ("2025-12-30", 4000.0),
            ("2026-01-02", 6000.0),
            ("2026-01-03", 1000.0),
        ]
        .iter()
        .map(|&(d, v)| Reading {
            point_id: 83022,
            ts: day_start(d) + 3600,
            value: Some(v),
        })
        .collect();
        db::upsert_readings(&pool, 7, &rows).await.unwrap();
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
