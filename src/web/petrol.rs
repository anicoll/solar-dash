//! The petrol prices API: list, add or replace, and remove the entries of the
//! `petrol_price` table.
//!
//! The write routes sit behind `auth::require_session` like the rest of
//! `/api/`. What keeps another website from using a visitor's login to change
//! prices: writes need the session cookie (the `api_token` Bearer only works
//! for GET and HEAD, see `auth.rs`); the cookie is `SameSite=Lax`, so browsers
//! do not send it on cross-site PUT or DELETE requests (or on cross-site form
//! POSTs); a request whose `Origin` header names another host is refused with
//! 403 (see `same_origin` in `mod.rs`); a page cannot make a cross-site PUT
//! with a JSON body without a CORS preflight, which we never approve; and
//! `PUT` requires `Content-Type: application/json` (axum's `Json` answers 415
//! otherwise), which a plain HTML form cannot send.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::api::{ApiError, ApiResult};
use crate::admin_data::{self, log_change};
use crate::config::parse_date;
use crate::ev_cost::PetrolHistory;
use crate::petrol_prices::check_price;

/// `{"fallback_cents_per_litre":..,"prices":[{from,cents_per_litre},...]}`.
/// `prices` are oldest first.
pub async fn list(State(state): State<AppState>) -> ApiResult {
    Ok(Json(view_json(&state.admin.get().petrol)))
}

#[derive(Deserialize)]
pub struct NewPrice {
    from: String,
    cents_per_litre: f64,
}

/// Adds the price starting on `from`, or replaces the one already on that
/// date. Body: `{"from":"YYYY-MM-DD","cents_per_litre":189.9}`. Answers like
/// the list.
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
    let from = date(&body.from)?.to_string();
    let cents = body.cents_per_litre;
    check_price(cents).map_err(|err| ApiError::bad_request(err.to_string()))?;
    admin_data::write(&state, async |tx| {
        let before = price_row(tx, &from).await?;
        sqlx::query(
            "INSERT INTO petrol_price (from_date, cents_per_litre) VALUES (?, ?) \
             ON CONFLICT(from_date) DO UPDATE SET cents_per_litre = excluded.cents_per_litre",
        )
        .bind(&from)
        .bind(cents)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
        let after = json!({ "from": from, "cents_per_litre": cents });
        let action = if before.is_some() { "update" } else { "create" };
        log_change(
            tx,
            "petrol",
            action,
            Some(&from),
            before.as_ref(),
            Some(&after),
        )
        .await?;
        Ok(())
    })
    .await?;
    Ok(Json(view_json(&state.admin.get().petrol)))
}

/// Removes the price starting on the date in the path. 404 if there is none.
/// Answers like the list.
pub async fn remove(State(state): State<AppState>, Path(from): Path<String>) -> ApiResult {
    let from = date(&from)?.to_string();
    admin_data::write(&state, async |tx| {
        let Some(before) = price_row(tx, &from).await? else {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!("there is no petrol price starting on {from}"),
            ));
        };
        sqlx::query("DELETE FROM petrol_price WHERE from_date = ?")
            .bind(&from)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        log_change(tx, "petrol", "delete", Some(&from), Some(&before), None).await?;
        Ok(())
    })
    .await?;
    Ok(Json(view_json(&state.admin.get().petrol)))
}

/// The row for `from` as JSON, or `None` if there is none.
async fn price_row(tx: &mut sqlx::SqliteConnection, from: &str) -> Result<Option<Value>, ApiError> {
    let row: Option<(f64,)> =
        sqlx::query_as("SELECT cents_per_litre FROM petrol_price WHERE from_date = ?")
            .bind(from)
            .fetch_optional(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
    Ok(row.map(|(cents,)| json!({ "from": from, "cents_per_litre": cents })))
}

fn date(text: &str) -> Result<NaiveDate, ApiError> {
    parse_date(text).map_err(|err| ApiError::bad_request(format!("from: {err}")))
}

pub(super) fn view_json(petrol: &PetrolHistory) -> Value {
    let prices: Vec<Value> = petrol
        .changes()
        .iter()
        .map(|(from, cents)| json!({ "from": from, "cents_per_litre": cents }))
        .collect();
    json!({
        "fallback_cents_per_litre": petrol.fallback(),
        "prices": prices,
    })
}
