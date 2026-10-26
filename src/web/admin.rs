//! The admin API: read and edit the tariff plans, petrol prices, purchases,
//! past bills and settings that used to live in `config.toml`.
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
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::SqliteConnection;

use super::AppState;
use super::api::{ApiError, ApiResult, today};
use super::petrol::view_json;
use crate::admin_data::{
    self, AdminData, SETTING_KEYS, apply_setting, load_bills, load_settings, log_change,
    tariff_body,
};
use crate::bills::{self, PastBill};
use crate::config::{self, parse_date};

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
/// `{"tariffs":[Plan..],"petrol":{..},"purchases":[..],"bills":[..],
/// "settings":{..},"setting_defaults":{..},"changes":[..]}`. `settings` has
/// the keys that have been set; the others use their default, which
/// `setting_defaults` lists. With `?download=1` the answer is an attachment.
pub async fn read(
    State(state): State<AppState>,
    Query(params): Query<ReadParams>,
) -> Result<Response, ApiError> {
    let mut conn = state.pool().acquire().await.map_err(db_error)?;
    let tariffs = plans_json(&mut conn).await?;
    let purchases = purchases_json(&mut conn).await?;
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

/// The body of `POST` and `PUT` on a plan.
#[derive(Deserialize)]
pub struct PlanBody {
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
