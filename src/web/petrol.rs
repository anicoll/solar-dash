//! The petrol prices API: list, add or replace, and remove the entries of the
//! `petrol_prices_file`.
//!
//! The write routes sit behind `auth::require_session` like the rest of
//! `/api/`. What keeps another website from using a visitor's login to change
//! prices: the session cookie is `SameSite=Lax`, so browsers do not send it on
//! cross-site PUT or DELETE requests (or on cross-site form POSTs); a page
//! cannot make a cross-site PUT with a JSON body without a CORS preflight,
//! which we never approve; and `PUT` requires `Content-Type: application/json`
//! (axum's `Json` answers 415 otherwise), which a plain HTML form cannot send.
//! The Bearer token (`api_token`) is not sent by browsers on their own.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::api::{ApiError, ApiResult};
use crate::config::parse_date;
use crate::petrol_prices::{StoreError, View};

/// `{"file":bool,"fallback_cents_per_litre":..,"prices":[{from,cents_per_litre},...]}`.
/// `file` is false when the prices come from `[[teslamate.petrol_price]]`,
/// which cannot be changed here. `prices` are oldest first.
pub async fn list(State(state): State<AppState>) -> ApiResult {
    Ok(Json(view_json(&state.petrol.view())))
}

#[derive(Deserialize)]
pub struct NewPrice {
    from: String,
    cents_per_litre: f64,
}

/// Adds the price starting on `from`, or replaces the one already on that
/// date. Body: `{"from":"YYYY-MM-DD","cents_per_litre":189.9}`. Answers like
/// the list. 409 without a `petrol_prices_file`.
pub async fn put(
    State(state): State<AppState>,
    body: Result<Json<NewPrice>, JsonRejection>,
) -> ApiResult {
    let Json(body) = body.map_err(|rejection| {
        // Keep 415 for a wrong content type; every other problem is a 400.
        let status = match rejection.status() {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            _ => StatusCode::BAD_REQUEST,
        };
        ApiError::new(
            status,
            format!(
                "send JSON like {{\"from\":\"2026-03-01\",\"cents_per_litre\":189.9}}: {}",
                rejection.body_text()
            ),
        )
    })?;
    let from = date(&body.from)?;
    let view = state.petrol.put(from, body.cents_per_litre)?;
    Ok(Json(view_json(&view)))
}

/// Removes the price starting on the date in the path. 404 if there is none,
/// 409 without a `petrol_prices_file`. Answers like the list.
pub async fn remove(State(state): State<AppState>, Path(from): Path<String>) -> ApiResult {
    let from = date(&from)?;
    let view = state.petrol.remove(from)?;
    Ok(Json(view_json(&view)))
}

fn date(text: &str) -> Result<NaiveDate, ApiError> {
    parse_date(text).map_err(|err| ApiError::bad_request(format!("from: {err}")))
}

fn view_json(view: &View) -> Value {
    let prices: Vec<Value> = view
        .prices
        .iter()
        .map(|(from, cents)| json!({ "from": from.to_string(), "cents_per_litre": cents }))
        .collect();
    json!({
        "file": view.file,
        "fallback_cents_per_litre": view.fallback,
        "prices": prices,
    })
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotConfigured => ApiError::new(
                StatusCode::CONFLICT,
                "petrol prices are not kept in a file: set petrol_prices_file in [teslamate] \
                 in config.toml to change them here",
            ),
            StoreError::BadFile(message) => ApiError::new(StatusCode::CONFLICT, message),
            StoreError::NotFound(from) => ApiError::new(
                StatusCode::NOT_FOUND,
                format!("there is no petrol price starting on {from}"),
            ),
            StoreError::Invalid(message) => ApiError::bad_request(message),
            StoreError::Io(err) => ApiError::from(err),
        }
    }
}
