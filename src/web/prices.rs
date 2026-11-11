//! `GET /api/prices`: stored electricity prices (see `amber::store`).

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::api::{ApiError, ApiResult};
use crate::amber::store::{self, StoredPrice};

/// The longest range one request may ask for. Five-minute prices on three
/// channels are about 900 rows a day, so this is under 30,000 rows.
const MAX_PRICES_SECS: i64 = 31 * 24 * 3600;

#[derive(Debug, Deserialize)]
pub struct PricesParams {
    from: Option<String>,
    to: Option<String>,
    /// Only this source's prices, e.g. `amber:01F5A5CRKMZ5BCX9P1S4V990AM`.
    source: Option<String>,
}

fn parse_unix(name: &str, text: Option<&str>) -> Result<i64, ApiError> {
    text.and_then(|t| t.parse().ok())
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a unix time in seconds")))
}

/// Groups rows (already sorted by channel) into `{channel: [..]}`.
fn by_channel(rows: Vec<StoredPrice>) -> BTreeMap<String, Vec<StoredPrice>> {
    let mut grouped: BTreeMap<String, Vec<StoredPrice>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.channel.clone()).or_default().push(row);
    }
    grouped
}

/// `{"channels":{"general":[{start,end,cents,spot_cents,kind},...],"feed_in":[...]},"now":{"general":{...}}}`.
///
/// `from` and `to` are unix seconds, at most 31 days apart; intervals that
/// start in `[from, to)` are returned, oldest first, grouped by channel
/// (`general`, `feed_in`, `controlled_load`). `kind` is `actual`, `current`
/// or `forecast`. `now` has the interval each channel is in right now, whatever
/// the range, and is `{}` when there is none. Prices are c/kWh including GST,
/// as the source gave them: the feed-in sign is not changed.
pub async fn prices(
    State(state): State<AppState>,
    Query(params): Query<PricesParams>,
) -> ApiResult {
    let from = parse_unix("from", params.from.as_deref())?;
    let to = parse_unix("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_PRICES_SECS {
        return Err(ApiError::bad_request("the range can be at most 31 days"));
    }
    let source = params.source.as_deref();

    let rows = store::prices_between(&state.pool, source, from, to).await?;
    let current = store::prices_at(&state.pool, source, Utc::now().timestamp()).await?;
    // A channel can have rows from several sources; the query lists the
    // newest start first, so the first one per channel is the one to show.
    let mut now = BTreeMap::new();
    for row in current {
        now.entry(row.channel.clone()).or_insert(row);
    }
    Ok(Json(json!({ "channels": by_channel(rows), "now": now })))
}
