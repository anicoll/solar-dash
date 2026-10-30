//! The admin API: read and edit the tariff plans, petrol prices, purchases,
//! past bills and settings that used to live in `config.toml`, and the
//! manual cars with their petrol fills.
//!
//! Everything here sits behind `auth::require_session`, and the write routes
//! also need the session cookie (not the Bearer token) and a matching `Origin`;
//! see the top of `petrol.rs`. Every write goes through `admin_data::write`:
//! one transaction, the change plus its `admin_change` rows, a check of the
//! whole data set, then the shared snapshot is swapped so the rest of the API
//! shows the change at once. Errors are `{"error": "..."}`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{Days, NaiveDate};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::SqliteConnection;

use super::AppState;
use super::api::{ApiError, ApiResult, date_range, today};
use super::costs;
use super::petrol::view_json;
use crate::admin_data::{
    self, AdminData, SETTING_KEYS, apply_setting, load_bills, load_settings, log_change,
    tariff_body,
};
use crate::bills::{self, PastBill};
use crate::config::{self, parse_date};
use crate::cost::TariffSchedule;
use crate::db;

/// How many entries of the change log `GET /api/admin` lists.
const CHANGES_SHOWN: i64 = 50;

/// An unexpected database failure becomes a 500 (and is logged).
fn db_error(err: sqlx::Error) -> ApiError {
    ApiError::from(anyhow::Error::from(err))
}

/// The JSON body of a request, with a 415 kept for a wrong content type and
/// a 400 (with the reason) for anything else.
fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|rejection| {
        let status = match rejection.status() {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError::new(
            status,
            format!("send a JSON body: {}", rejection.body_text()),
        )
    })
}

fn not_found(what: &str, key: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, format!("there is no {what} {key}"))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ReadParams {
    /// `1` sends the answer as a file to save (the Export button).
    download: Option<String>,
}

/// Everything the admin page edits, plus the latest changes:
/// `{"tariffs":[Plan..],"petrol":{..},"purchases":[..],"manual_cars":[..],
/// "bills":[..],"settings":{..},"setting_defaults":{..},"changes":[..]}`. `settings` has
/// the keys that have been set; the others use their default, which
/// `setting_defaults` lists. With `?download=1` the answer is an attachment.
pub async fn read(
    State(state): State<AppState>,
    Query(params): Query<ReadParams>,
) -> Result<Response, ApiError> {
    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    let tariffs = plans_json(&mut conn).await?;
    let purchases = purchases_json(&mut conn).await?;
    let cars = cars_json(&mut conn).await?;
    let bills: Vec<Value> = load_bills(&mut conn).await?.iter().map(bill_json).collect();
    let settings = load_settings(&mut conn).await?;
    let changes: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, at, what, action, key FROM admin_change ORDER BY id DESC LIMIT ?",
    )
    .bind(CHANGES_SHOWN)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    let changes: Vec<Value> = changes
        .into_iter()
        .map(|(id, at, what, action, key)| {
            json!({ "id": id, "at": at, "what": what, "action": action, "key": key })
        })
        .collect();

    let json = Json(json!({
        "tariffs": tariffs,
        "petrol": view_json(&state.admin().get().petrol),
        "purchases": purchases,
        "manual_cars": cars,
        "bills": bills,
        "settings": settings,
        "setting_defaults": setting_defaults(),
        "changes": changes,
    }));
    if params.download.as_deref() != Some("1") {
        return Ok(json.into_response());
    }
    let mut response = json.into_response();
    let name = format!("solar-dash-admin-{}.json", today(state.tz()));
    let disposition = format!("attachment; filename=\"{name}\"");
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).expect("the file name is plain ASCII"),
    );
    Ok(response)
}

/// The newest nightly backup: `{"latest":{"date":"YYYY-MM-DD","bytes":n}}`, or
/// `{"latest":null}` when there is none (or backups are off and the folder is
/// empty).
pub async fn backup_info(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let Some((date, path)) = newest_backup(&state).await? else {
        return Ok(Json(json!({ "latest": null })));
    };
    let bytes = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        // Pruned between the listing and now: treat as none.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Json(json!({ "latest": null })));
        }
        Err(err) => return Err(anyhow::Error::from(err).into()),
    };
    Ok(Json(
        json!({ "latest": { "date": date.to_string(), "bytes": bytes } }),
    ))
}

/// The newest backup file as an attachment. Session cookie only (see
/// `auth::require_session`). 404 when there is no backup.
pub async fn backup_download(State(state): State<AppState>) -> Result<Response, ApiError> {
    let none = || ApiError::new(StatusCode::NOT_FOUND, "there is no backup yet".to_string());
    let (date, path) = newest_backup(&state).await?.ok_or_else(none)?;
    // The file is tens of MB, so read it off the async threads.
    let read = tokio::task::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(anyhow::Error::from)?;
    let bytes = match read {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Err(none()),
        Err(err) => return Err(anyhow::Error::from(err).into()),
    };
    let disposition = format!("attachment; filename=\"solar-{date}.db\"");
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

async fn newest_backup(
    state: &AppState,
) -> Result<Option<(NaiveDate, std::path::PathBuf)>, ApiError> {
    let Some(dir) = state.backup_dir().map(|d| d.to_path_buf()) else {
        return Ok(None);
    };
    let found = tokio::task::spawn_blocking(move || crate::backup::latest(&dir))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(found)
}

/// What each setting is when it has no row.
fn setting_defaults() -> Value {
    let d = AdminData::default();
    json!({
        "petrol_cents_per_litre": d.ev_prices.petrol_cents_per_litre,
        "petrol_litres_per_100km": d.ev_prices.petrol_litres_per_100km,
        "away_cents_per_kwh": d.ev_prices.away_cents_per_kwh,
        "compare_cars": d.compare_cars,
        "petrol_grade": d.petrol_grade,
        "price_rise_percent": d.price_rise_percent,
        "bill_start_day": d.bill_start_day,
        "grid_pull_warn_kw": d.grid_pull_warn_kw,
        "grid_pull_limit_kw": d.grid_pull_limit_kw,
    })
}

// ---------------------------------------------------------------------------
// Tariff plans
// ---------------------------------------------------------------------------

/// A plan as the API sends it: `{"id":1,"valid_from":"2026-05-26"|null,"tariff":{..}}`.
fn plan_json(id: i64, valid_from: Option<&str>, body: &str) -> Result<Value, ApiError> {
    let tariff: Value = serde_json::from_str(body).map_err(anyhow::Error::from)?;
    Ok(json!({ "id": id, "valid_from": valid_from, "tariff": tariff }))
}

async fn plans_json(conn: &mut SqliteConnection) -> Result<Vec<Value>, ApiError> {
    let rows: Vec<(i64, Option<String>, String)> =
        sqlx::query_as("SELECT id, valid_from, body FROM tariff_plan ORDER BY valid_from, id")
            .fetch_all(&mut *conn)
            .await
            .map_err(db_error)?;
    rows.iter()
        .map(|(id, from, body)| plan_json(*id, from.as_deref(), body))
        .collect()
}

async fn plan_row(conn: &mut SqliteConnection, id: i64) -> Result<Option<Value>, ApiError> {
    let row: Option<(Option<String>, String)> =
        sqlx::query_as("SELECT valid_from, body FROM tariff_plan WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?;
    row.map(|(from, body)| plan_json(id, from.as_deref(), &body))
        .transpose()
}

/// The body of `POST`, `PUT` and the preview on a plan.
#[derive(Deserialize)]
pub struct PlanBody {
    /// Only the preview looks at this: the plan it replaces.
    id: Option<i64>,
    valid_from: Option<String>,
    tariff: config::Tariff,
}

/// Checks a plan's date and tariff; gives the date as stored and the body JSON.
fn checked_plan(plan: &PlanBody) -> Result<(Option<String>, String), ApiError> {
    let valid_from = plan
        .valid_from
        .as_deref()
        .map(parse_date)
        .transpose()
        .map_err(|err| ApiError::bad_request(format!("valid_from: {err:#}")))?
        .map(|date| date.to_string());
    plan.tariff
        .check("the tariff")
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    let body = tariff_body(&plan.tariff)?;
    Ok((valid_from, body))
}

/// Another plan that already starts on `valid_from`, if any.
async fn other_plan_on(
    conn: &mut SqliteConnection,
    valid_from: &Option<String>,
    except: i64,
) -> Result<(), ApiError> {
    let Some(date) = valid_from else {
        return Ok(());
    };
    let other: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM tariff_plan WHERE valid_from = ? AND id != ?")
            .bind(date)
            .bind(except)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?;
    match other {
        Some(_) => Err(ApiError::bad_request(format!(
            "another plan already starts on {date}"
        ))),
        None => Ok(()),
    }
}

/// `POST /api/admin/tariffs`: adds a plan and answers with it (201).
pub async fn tariff_create(
    State(state): State<AppState>,
    plan: Result<Json<PlanBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let plan = body(plan)?;
    let (valid_from, text) = checked_plan(&plan)?;
    let mut id = 0;
    admin_data::write(&state, async |tx| {
        other_plan_on(tx, &valid_from, 0).await?;
        let result = sqlx::query("INSERT INTO tariff_plan (valid_from, body) VALUES (?, ?)")
            .bind(&valid_from)
            .bind(&text)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        id = result.last_insert_rowid();
        let after = plan_json(id, valid_from.as_deref(), &text)?;
        log_change(
            tx,
            "tariffs",
            "create",
            Some(&id.to_string()),
            None,
            Some(&after),
        )
        .await?;
        Ok(())
    })
    .await?;
    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    let created = plan_row(&mut conn, id)
        .await?
        .expect("the plan was just saved");
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PUT /api/admin/tariffs/{id}`: replaces a plan and answers with it.
pub async fn tariff_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    plan: Result<Json<PlanBody>, JsonRejection>,
) -> ApiResult {
    let plan = body(plan)?;
    let (valid_from, text) = checked_plan(&plan)?;
    admin_data::write(&state, async |tx| {
        let Some(before) = plan_row(tx, id).await? else {
            return Err(not_found("tariff plan", id));
        };
        other_plan_on(tx, &valid_from, id).await?;
        sqlx::query("UPDATE tariff_plan SET valid_from = ?, body = ? WHERE id = ?")
            .bind(&valid_from)
            .bind(&text)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let after = plan_json(id, valid_from.as_deref(), &text)?;
        let key = id.to_string();
        log_change(
            tx,
            "tariffs",
            "update",
            Some(&key),
            Some(&before),
            Some(&after),
        )
        .await?;
        Ok(())
    })
    .await?;
    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    let updated = plan_row(&mut conn, id)
        .await?
        .expect("the plan was just saved");
    Ok(Json(updated))
}

/// `DELETE /api/admin/tariffs/{id}`: answers `{"deleted": Plan}`. Deleting the
/// last plan turns the cost features off.
pub async fn tariff_delete(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let mut deleted = Value::Null;
    admin_data::write(&state, async |tx| {
        let Some(before) = plan_row(tx, id).await? else {
            return Err(not_found("tariff plan", id));
        };
        sqlx::query("DELETE FROM tariff_plan WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        log_change(
            tx,
            "tariffs",
            "delete",
            Some(&id.to_string()),
            Some(&before),
            None,
        )
        .await?;
        deleted = before;
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
pub struct PreviewParams {
    from: Option<String>,
    to: Option<String>,
}

/// The longest range a preview prices.
const MAX_PREVIEW_DAYS: u64 = 92;
/// The preview covers this many days ending yesterday unless asked otherwise.
const PREVIEW_DAYS: u64 = 30;

/// `POST /api/admin/tariffs/preview`: what the days so far would have cost
/// with this plan, next to what they cost on the current plans. The body is a
/// plan; with an `id` it replaces that plan, without one it is added. Nothing
/// is saved, and no cost is stored.
///
/// Answers `{"days":[{"date","current_cents","new_cents"}..],"total":{..}}`
/// for the last 30 complete days, or `?from=&to=` (at most 92 days). Days with
/// no data are left out. With no plan saved yet, `current_cents` is null.
pub async fn tariff_preview(
    State(state): State<AppState>,
    Query(params): Query<PreviewParams>,
    plan: Result<Json<PlanBody>, JsonRejection>,
) -> ApiResult {
    let plan = body(plan)?;
    checked_plan(&plan)?;
    let valid_from = plan
        .valid_from
        .as_deref()
        .map(parse_date)
        .transpose()
        .map_err(|err| ApiError::bad_request(format!("valid_from: {err:#}")))?;

    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    let rows = admin_data::load_plan_rows(&mut conn).await?;
    let current: Vec<_> = rows
        .iter()
        .map(|(_, from, tariff)| (*from, tariff.clone()))
        .collect();
    let mut changed = current.clone();
    match plan.id {
        Some(id) => {
            let Some(i) = rows.iter().position(|(row_id, _, _)| *row_id == id) else {
                return Err(not_found("tariff plan", id));
            };
            changed[i] = (valid_from, plan.tariff);
        }
        None => changed.push((valid_from, plan.tariff)),
    }
    let changed = TariffSchedule::from_plans(&changed)
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    let current = if current.is_empty() {
        None
    } else {
        Some(TariffSchedule::from_plans(&current)?)
    };

    // The default range ends yesterday: today is not finished yet.
    let to = params.to.clone().or_else(|| {
        params
            .from
            .is_none()
            .then(|| (today(state.tz) - Days::new(1)).to_string())
    });
    let (from, count) = date_range(
        params.from.as_deref(),
        to.as_deref(),
        state.tz,
        MAX_PREVIEW_DAYS,
        PREVIEW_DAYS,
    )?;
    let dates: Vec<NaiveDate> = from.iter_days().take(count as usize).collect();

    let new_days = costs::preview_days(&state, &changed, &dates).await?;
    let current_days = match &current {
        Some(schedule) => Some(costs::preview_days(&state, schedule, &dates).await?),
        None => None,
    };
    let mut total_new = 0.0;
    let mut total_current = 0.0;
    let mut days = Vec::new();
    for (i, day) in new_days.iter().enumerate() {
        // Both lists come from the same intervals, so they have the same days.
        let was = current_days.as_ref().map(|list| list[i].cost.actual_cents);
        total_new += day.cost.actual_cents;
        total_current += was.unwrap_or(0.0);
        days.push(json!({
            "date": day.date.to_string(),
            "current_cents": was.map(costs::round2),
            "new_cents": costs::round2(day.cost.actual_cents),
        }));
    }
    Ok(Json(json!({
        "days": days,
        "total": {
            "current_cents": current_days.as_ref().map(|_| costs::round2(total_current)),
            "new_cents": costs::round2(total_new),
        },
    })))
}

// ---------------------------------------------------------------------------
// Purchases
// ---------------------------------------------------------------------------

fn purchase_json(id: i64, p: &config::Purchase) -> Value {
    json!({
        "id": id,
        "name": p.name,
        "date": p.date,
        "cost_dollars": p.cost_dollars,
        "battery_kwh": p.battery_kwh,
    })
}

async fn purchase_row(conn: &mut SqliteConnection, id: i64) -> Result<Option<Value>, ApiError> {
    let row: Option<(String, String, f64, Option<f64>)> =
        sqlx::query_as("SELECT name, date, cost_dollars, battery_kwh FROM purchase WHERE id = ?")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_error)?;
    Ok(row.map(|(name, date, cost_dollars, battery_kwh)| {
        purchase_json(
            id,
            &config::Purchase {
                name,
                date,
                cost_dollars,
                battery_kwh,
            },
        )
    }))
}

async fn purchases_json(conn: &mut SqliteConnection) -> Result<Vec<Value>, ApiError> {
    let rows: Vec<(i64, String, String, f64, Option<f64>)> = sqlx::query_as(
        "SELECT id, name, date, cost_dollars, battery_kwh FROM purchase ORDER BY date, id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    Ok(rows
        .into_iter()
        .map(|(id, name, date, cost_dollars, battery_kwh)| {
            purchase_json(
                id,
                &config::Purchase {
                    name,
                    date,
                    cost_dollars,
                    battery_kwh,
                },
            )
        })
        .collect())
}

/// The body of `POST` and `PUT` on a purchase:
/// `{"name":..,"date":"YYYY-MM-DD","cost_dollars":..,"battery_kwh":..}`.
#[derive(Deserialize)]
pub struct PurchaseBody {
    name: String,
    date: String,
    cost_dollars: f64,
    #[serde(default)]
    battery_kwh: Option<f64>,
}

/// Checks the purchase like the config did, and tidies the name and date.
fn checked_purchase(body: PurchaseBody) -> Result<config::Purchase, ApiError> {
    let purchase = config::Purchase {
        name: body.name.trim().to_string(),
        date: body.date,
        cost_dollars: body.cost_dollars,
        battery_kwh: body.battery_kwh,
    };
    purchase
        .check("purchase")
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(purchase)
}

/// `POST /api/admin/purchases`: adds a purchase and answers with it (201).
pub async fn purchase_create(
    State(state): State<AppState>,
    purchase: Result<Json<PurchaseBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let purchase = checked_purchase(body(purchase)?)?;
    let mut id = 0;
    admin_data::write(&state, async |tx| {
        let result = sqlx::query(
            "INSERT INTO purchase (name, date, cost_dollars, battery_kwh) VALUES (?, ?, ?, ?)",
        )
        .bind(&purchase.name)
        .bind(&purchase.date)
        .bind(purchase.cost_dollars)
        .bind(purchase.battery_kwh)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        id = result.last_insert_rowid();
        let after = purchase_json(id, &purchase);
        log_change(
            tx,
            "purchases",
            "create",
            Some(&id.to_string()),
            None,
            Some(&after),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(purchase_json(id, &purchase))))
}

/// `PUT /api/admin/purchases/{id}`: replaces a purchase and answers with it.
pub async fn purchase_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    purchase: Result<Json<PurchaseBody>, JsonRejection>,
) -> ApiResult {
    let purchase = checked_purchase(body(purchase)?)?;
    admin_data::write(&state, async |tx| {
        let Some(before) = purchase_row(tx, id).await? else {
            return Err(not_found("purchase", id));
        };
        sqlx::query(
            "UPDATE purchase SET name = ?, date = ?, cost_dollars = ?, battery_kwh = ? \
             WHERE id = ?",
        )
        .bind(&purchase.name)
        .bind(&purchase.date)
        .bind(purchase.cost_dollars)
        .bind(purchase.battery_kwh)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        let after = purchase_json(id, &purchase);
        let key = id.to_string();
        log_change(
            tx,
            "purchases",
            "update",
            Some(&key),
            Some(&before),
            Some(&after),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok(Json(purchase_json(id, &purchase)))
}

/// `DELETE /api/admin/purchases/{id}`: answers `{"deleted": Purchase}`.
pub async fn purchase_delete(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let mut deleted = Value::Null;
    admin_data::write(&state, async |tx| {
        let Some(before) = purchase_row(tx, id).await? else {
            return Err(not_found("purchase", id));
        };
        sqlx::query("DELETE FROM purchase WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let key = id.to_string();
        log_change(tx, "purchases", "delete", Some(&key), Some(&before), None).await?;
        deleted = before;
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Manual cars and their petrol fills
// ---------------------------------------------------------------------------

/// The kinds of car a manual car can be (display only).
const CAR_KINDS: [&str; 4] = ["phev", "petrol", "hybrid", "diesel"];

/// A fill of 200 litres or more is a typo.
const MAX_FILL_LITRES: f64 = 200.0;

fn car_json(car: &db::ManualCar) -> Value {
    json!({
        "id": car.id,
        "name": car.name,
        "model": car.model,
        "kind": car.kind,
        "owned_since": car.owned_since,
    })
}

fn fill_json(fill: &db::FuelFill) -> Value {
    json!({
        "id": fill.id,
        "car_id": fill.car_id,
        "date": fill.date,
        "litres": fill.litres,
        "cost_dollars": fill.cost_dollars,
        "odometer_km": fill.odometer_km,
        "full_tank": fill.full_tank,
        "note": fill.note,
    })
}

/// A manual car with its fill count: id, name, model, kind, owned since, fills.
type CarListRow = (i64, String, Option<String>, String, Option<String>, i64);

/// The manual cars for `GET /api/admin`, each with how many fills it has.
async fn cars_json(conn: &mut SqliteConnection) -> Result<Vec<Value>, ApiError> {
    let rows: Vec<CarListRow> = sqlx::query_as(
        "SELECT c.id, c.name, c.model, c.kind, c.owned_since, \
         (SELECT COUNT(*) FROM fuel_fill f WHERE f.car_id = c.id) \
         FROM manual_car c ORDER BY c.id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)?;
    Ok(rows
        .into_iter()
        .map(|(id, name, model, kind, owned_since, fills)| {
            json!({
                "id": id, "name": name, "model": model, "kind": kind,
                "owned_since": owned_since, "fills": fills,
            })
        })
        .collect())
}

async fn car_row(conn: &mut SqliteConnection, id: i64) -> Result<Option<db::ManualCar>, ApiError> {
    sqlx::query_as(
        "SELECT id, name, model, kind, created_at, owned_since FROM manual_car WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)
}

async fn fills_of_car(
    conn: &mut SqliteConnection,
    car_id: i64,
) -> Result<Vec<db::FuelFill>, ApiError> {
    sqlx::query_as(
        "SELECT id, car_id, date, litres, cost_dollars, odometer_km, full_tank, note \
         FROM fuel_fill WHERE car_id = ? ORDER BY date, id",
    )
    .bind(car_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_error)
}

async fn fill_row(conn: &mut SqliteConnection, id: i64) -> Result<Option<db::FuelFill>, ApiError> {
    sqlx::query_as(
        "SELECT id, car_id, date, litres, cost_dollars, odometer_km, full_tank, note \
         FROM fuel_fill WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(db_error)
}

/// The body of `POST` and `PUT` on a manual car:
/// `{"name":..,"model":..,"kind":"phev","owned_since":"YYYY-MM-DD"}`. `model`
/// is optional; `kind` is optional too (a new car is a `phev`, an update keeps
/// the kind it has). `owned_since` is optional and may be null: a `PUT` that
/// leaves it out or null clears the date.
#[derive(Deserialize)]
pub struct CarBody {
    name: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    owned_since: Option<String>,
}

/// A car's checked fields: name, model, kind and the day it was bought.
type CheckedCar = (String, Option<String>, String, Option<String>);

/// Checks the day a car was bought: a valid YYYY-MM-DD, not after `today` (the
/// local day). Gives the date as stored.
fn checked_owned_since(date: &str, today: NaiveDate) -> Result<String, ApiError> {
    let date = parse_date(date).map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    if date > today {
        return Err(ApiError::bad_request(format!(
            "the date {date} is in the future"
        )));
    }
    Ok(date.to_string())
}

/// Checks the car and tidies the text. `default_kind` is used when the body
/// has none.
fn checked_car(
    body: CarBody,
    default_kind: &str,
    today: NaiveDate,
) -> Result<CheckedCar, ApiError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("the car needs a name"));
    }
    let model = body
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let kind = body.kind.unwrap_or_else(|| default_kind.to_string());
    if !CAR_KINDS.contains(&kind.as_str()) {
        return Err(ApiError::bad_request(format!(
            "kind must be one of {}",
            CAR_KINDS.join(", ")
        )));
    }
    let owned_since = body
        .owned_since
        .map(|date| checked_owned_since(&date, today))
        .transpose()?;
    Ok((name, model, kind, owned_since))
}

/// `POST /api/admin/cars`: adds a manual car and answers with it (201).
pub async fn car_create(
    State(state): State<AppState>,
    car: Result<Json<CarBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (name, model, kind, owned_since) = checked_car(body(car)?, "phev", today(state.tz()))?;
    let mut created = Value::Null;
    admin_data::write(&state, async |tx| {
        let result = sqlx::query(
            "INSERT INTO manual_car (name, model, kind, owned_since, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&name)
        .bind(&model)
        .bind(&kind)
        .bind(&owned_since)
        .bind(db::now())
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        let id = result.last_insert_rowid();
        let Some(car) = car_row(tx, id).await? else {
            return Err(not_found("car", id));
        };
        created = car_json(&car);
        log_change(
            tx,
            "cars",
            "create",
            Some(&id.to_string()),
            None,
            Some(&created),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PUT /api/admin/cars/{id}`: replaces a manual car's details and answers
/// with it.
pub async fn car_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    car: Result<Json<CarBody>, JsonRejection>,
) -> ApiResult {
    let car = body(car)?;
    let mut updated = Value::Null;
    admin_data::write(&state, async |tx| {
        let Some(before) = car_row(tx, id).await? else {
            return Err(not_found("car", id));
        };
        let (name, model, kind, owned_since) = checked_car(
            CarBody {
                name: car.name.clone(),
                model: car.model.clone(),
                kind: car.kind.clone(),
                owned_since: car.owned_since.clone(),
            },
            &before.kind,
            today(state.tz()),
        )?;
        sqlx::query(
            "UPDATE manual_car SET name = ?, model = ?, kind = ?, owned_since = ? WHERE id = ?",
        )
        .bind(&name)
        .bind(&model)
        .bind(&kind)
        .bind(&owned_since)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        updated = json!({
            "id": id, "name": name, "model": model, "kind": kind, "owned_since": owned_since,
        });
        let key = id.to_string();
        log_change(
            tx,
            "cars",
            "update",
            Some(&key),
            Some(&car_json(&before)),
            Some(&updated),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok(Json(updated))
}

/// The body of `PUT /api/admin/teslamate-cars/{id}/owned-since`:
/// `{"owned_since":"YYYY-MM-DD"}`, or null to clear the date.
#[derive(Deserialize)]
pub struct OwnedSinceBody {
    // No `default`: leaving the key out is an error, only null clears.
    #[serde(deserialize_with = "Option::deserialize")]
    owned_since: Option<String>,
}

/// `PUT /api/admin/teslamate-cars/{id}/owned-since`: sets the day a TeslaMate
/// car was bought, or clears it with null. Answers
/// `{"car_id":id,"owned_since":"YYYY-MM-DD"|null}`. TeslaMate has no purchase
/// date, so it is kept in `tm_car_owned`, which the sync leaves alone. The id
/// is not checked against `tm_car` (the sync may not have run yet).
pub async fn teslamate_car_owned_since(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    owned: Result<Json<OwnedSinceBody>, JsonRejection>,
) -> ApiResult {
    let owned_since = body(owned)?
        .owned_since
        .map(|date| checked_owned_since(&date, today(state.tz())))
        .transpose()?;
    let answer = json!({ "car_id": id, "owned_since": owned_since });
    admin_data::write(&state, async |tx| {
        let before: Option<String> =
            sqlx::query_scalar("SELECT owned_since FROM tm_car_owned WHERE car_id = ?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_error)?;
        match &owned_since {
            Some(date) => {
                sqlx::query(
                    "INSERT INTO tm_car_owned (car_id, owned_since) VALUES (?, ?) \
                     ON CONFLICT(car_id) DO UPDATE SET owned_since = excluded.owned_since",
                )
                .bind(id)
                .bind(date)
                .execute(&mut *tx)
                .await
                .map_err(db_error)?;
            }
            None => {
                sqlx::query("DELETE FROM tm_car_owned WHERE car_id = ?")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_error)?;
            }
        }
        let key = id.to_string();
        let before = json!({ "car_id": id, "owned_since": before });
        log_change(
            tx,
            "teslamate_cars",
            "update",
            Some(&key),
            Some(&before),
            Some(&answer),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok(Json(answer))
}

/// `DELETE /api/admin/cars/{id}`: removes a manual car and its fills. Answers
/// `{"deleted": Car, "fills_deleted": n}`. The change log keeps the fills.
pub async fn car_delete(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let mut deleted = Value::Null;
    let mut fills_deleted = 0;
    admin_data::write(&state, async |tx| {
        let Some(car) = car_row(tx, id).await? else {
            return Err(not_found("car", id));
        };
        let fills: Vec<Value> = fills_of_car(tx, id).await?.iter().map(fill_json).collect();
        // The fills go with the car (`ON DELETE CASCADE`).
        sqlx::query("DELETE FROM manual_car WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let mut before = car_json(&car);
        before["fills"] = Value::Array(fills.clone());
        let key = id.to_string();
        log_change(tx, "cars", "delete", Some(&key), Some(&before), None).await?;
        deleted = car_json(&car);
        fills_deleted = fills.len();
        Ok(())
    })
    .await?;
    Ok(Json(
        json!({ "deleted": deleted, "fills_deleted": fills_deleted }),
    ))
}

/// The body of `POST` and `PUT` on a fill: `{"date":"YYYY-MM-DD","litres":..,
/// "cost_dollars":..,"odometer_km":..,"full_tank":true,"note":".."}`. The
/// odometer and note are optional; `full_tank` is true unless said otherwise.
#[derive(Deserialize)]
pub struct FillBody {
    date: String,
    litres: f64,
    cost_dollars: f64,
    #[serde(default)]
    odometer_km: Option<f64>,
    #[serde(default = "full_by_default")]
    full_tank: bool,
    #[serde(default)]
    note: String,
}

fn full_by_default() -> bool {
    true
}

/// Checks a fill (a fill-up cannot be in the future, the local day being
/// what counts) and tidies the note. `car_id` is not checked here.
fn checked_fill(body: FillBody, today: NaiveDate) -> Result<FillBody, ApiError> {
    let date = parse_date(&body.date).map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    if date > today {
        return Err(ApiError::bad_request(format!(
            "the date {date} is in the future"
        )));
    }
    // The comparisons are false for NaN, so that is rejected too.
    if !(body.litres > 0.0 && body.litres < MAX_FILL_LITRES) {
        return Err(ApiError::bad_request(format!(
            "litres must be more than 0 and less than {MAX_FILL_LITRES}"
        )));
    }
    if !(body.cost_dollars >= 0.0 && body.cost_dollars.is_finite()) {
        return Err(ApiError::bad_request("cost_dollars must be zero or more"));
    }
    if let Some(km) = body.odometer_km
        && !(km >= 0.0 && km.is_finite())
    {
        return Err(ApiError::bad_request("odometer_km must be zero or more"));
    }
    Ok(FillBody {
        note: body.note.trim().to_string(),
        ..body
    })
}

/// `POST /api/admin/cars/{id}/fills`: adds a fill to a car and answers with
/// it (201).
pub async fn fill_create(
    State(state): State<AppState>,
    Path(car_id): Path<i64>,
    fill: Result<Json<FillBody>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let fill = checked_fill(body(fill)?, today(state.tz()))?;
    let mut created = Value::Null;
    admin_data::write(&state, async |tx| {
        if car_row(tx, car_id).await?.is_none() {
            return Err(not_found("car", car_id));
        }
        let result = sqlx::query(
            "INSERT INTO fuel_fill (car_id, date, litres, cost_dollars, odometer_km, full_tank, \
             note) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(car_id)
        .bind(&fill.date)
        .bind(fill.litres)
        .bind(fill.cost_dollars)
        .bind(fill.odometer_km)
        .bind(fill.full_tank)
        .bind(&fill.note)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        let id = result.last_insert_rowid();
        created = fill_json(&db::FuelFill {
            id,
            car_id,
            date: fill.date.clone(),
            litres: fill.litres,
            cost_dollars: fill.cost_dollars,
            odometer_km: fill.odometer_km,
            full_tank: fill.full_tank,
            note: fill.note.clone(),
        });
        log_change(
            tx,
            "fills",
            "create",
            Some(&id.to_string()),
            None,
            Some(&created),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `PUT /api/admin/fills/{id}`: replaces a fill (it stays with its car) and
/// answers with it.
pub async fn fill_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    fill: Result<Json<FillBody>, JsonRejection>,
) -> ApiResult {
    let fill = checked_fill(body(fill)?, today(state.tz()))?;
    let mut updated = Value::Null;
    admin_data::write(&state, async |tx| {
        let Some(before) = fill_row(tx, id).await? else {
            return Err(not_found("fill", id));
        };
        sqlx::query(
            "UPDATE fuel_fill SET date = ?, litres = ?, cost_dollars = ?, odometer_km = ?, \
             full_tank = ?, note = ? WHERE id = ?",
        )
        .bind(&fill.date)
        .bind(fill.litres)
        .bind(fill.cost_dollars)
        .bind(fill.odometer_km)
        .bind(fill.full_tank)
        .bind(&fill.note)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        updated = fill_json(&db::FuelFill {
            id,
            car_id: before.car_id,
            date: fill.date.clone(),
            litres: fill.litres,
            cost_dollars: fill.cost_dollars,
            odometer_km: fill.odometer_km,
            full_tank: fill.full_tank,
            note: fill.note.clone(),
        });
        let key = id.to_string();
        log_change(
            tx,
            "fills",
            "update",
            Some(&key),
            Some(&fill_json(&before)),
            Some(&updated),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok(Json(updated))
}

/// `DELETE /api/admin/fills/{id}`: answers `{"deleted": Fill}`.
pub async fn fill_delete(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult {
    let mut deleted = Value::Null;
    admin_data::write(&state, async |tx| {
        let Some(before) = fill_row(tx, id).await? else {
            return Err(not_found("fill", id));
        };
        sqlx::query("DELETE FROM fuel_fill WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        deleted = fill_json(&before);
        let key = id.to_string();
        log_change(tx, "fills", "delete", Some(&key), Some(&deleted), None).await?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Bills
// ---------------------------------------------------------------------------

/// A bill with the same field names as the bills CSV (`from` and `to` for the dates).
fn bill_json(bill: &PastBill) -> Value {
    json!({
        "from": bill.from.to_string(),
        "to": bill.to.to_string(),
        "bill_total": bill.bill_total,
        "excluded": bill.excluded,
        "import_kwh": bill.import_kwh,
        "import_dollars": bill.import_dollars,
        "export_kwh": bill.export_kwh,
        "export_dollars": bill.export_dollars,
        "supply_dollars": bill.supply_dollars,
        "note": bill.note,
    })
}

/// `PUT /api/admin/bills` with `Content-Type: text/csv`: the body is a whole
/// bills CSV (the format of [`bills::parse`]) that replaces every bill.
/// Answers `{"rows": n}`.
pub async fn bills_replace(
    State(state): State<AppState>,
    headers: HeaderMap,
    text: String,
) -> ApiResult {
    let is_csv = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/csv"));
    if !is_csv {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send the bills as a CSV file with Content-Type: text/csv",
        ));
    }
    let list = bills::parse(&text).map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    admin_data::write(&state, async |tx| {
        let (before,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bill")
            .fetch_one(&mut *tx)
            .await
            .map_err(db_error)?;
        sqlx::query("DELETE FROM bill")
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        for bill in &list {
            insert_bill(tx, bill).await?;
        }
        let (before, after) = (json!({ "rows": before }), json!({ "rows": list.len() }));
        log_change(tx, "bills", "replace", None, Some(&before), Some(&after)).await?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "rows": list.len() })))
}

async fn insert_bill(conn: &mut SqliteConnection, b: &PastBill) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO bill (from_date, to_date, bill_total, excluded, import_kwh, \
         import_dollars, export_kwh, export_dollars, supply_dollars, note) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(b.from.to_string())
    .bind(b.to.to_string())
    .bind(b.bill_total)
    .bind(b.excluded)
    .bind(b.import_kwh)
    .bind(b.import_dollars)
    .bind(b.export_kwh)
    .bind(b.export_dollars)
    .bind(b.supply_dollars)
    .bind(&b.note)
    .execute(&mut *conn)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// `DELETE /api/admin/bills/{from}`: removes the bill starting that day and
/// answers `{"deleted": Bill}`.
pub async fn bill_delete(State(state): State<AppState>, Path(from): Path<String>) -> ApiResult {
    let from = parse_date(&from)
        .map_err(|err| ApiError::bad_request(format!("from: {err:#}")))?
        .to_string();
    let mut deleted = Value::Null;
    admin_data::write(&state, async |tx| {
        let bills = load_bills(tx).await?;
        let Some(before) = bills.iter().find(|b| b.from.to_string() == from) else {
            return Err(not_found("bill starting on", &from));
        };
        sqlx::query("DELETE FROM bill WHERE from_date = ?")
            .bind(&from)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        let before = bill_json(before);
        log_change(tx, "bills", "delete", Some(&from), Some(&before), None).await?;
        deleted = before;
        Ok(())
    })
    .await?;
    Ok(Json(json!({ "deleted": deleted })))
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// `PUT /api/admin/settings`: a partial object of setting keys. `null` resets
/// a key to its default (its row is deleted). An unknown key or a bad value
/// is a 400 and nothing changes. Answers with the settings that are set.
pub async fn settings_update(
    State(state): State<AppState>,
    changes: Result<Json<Map<String, Value>>, JsonRejection>,
) -> ApiResult {
    let changes = body(changes)?;
    // Check every value before touching the database.
    for (key, value) in &changes {
        if !SETTING_KEYS.contains(&key.as_str()) {
            return Err(ApiError::bad_request(format!(
                "unknown setting {key:?}; the settings are {}",
                SETTING_KEYS.join(", ")
            )));
        }
        if !value.is_null() {
            apply_setting(&mut AdminData::default(), key, value)
                .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
        }
    }
    admin_data::write(&state, async |tx| {
        let stored = load_settings(tx).await?;
        for (key, value) in &changes {
            let before = stored.get(key);
            if value.is_null() {
                if before.is_some() {
                    sqlx::query("DELETE FROM setting WHERE key = ?")
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_error)?;
                    log_change(tx, "settings", "delete", Some(key), before, None).await?;
                }
                continue;
            }
            if before == Some(value) {
                continue;
            }
            sqlx::query(
                "INSERT INTO setting (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
            let action = if before.is_some() { "update" } else { "create" };
            log_change(tx, "settings", action, Some(key), before, Some(value)).await?;
        }
        Ok(())
    })
    .await?;
    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    Ok(Json(json!(load_settings(&mut conn).await?)))
}
