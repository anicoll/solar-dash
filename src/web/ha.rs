//! `GET /api/ha/entities` and `GET /api/ha/history`: what the Home Assistant
//! source (see `ha_source`) has read. Both only read; nothing here talks to
//! Home Assistant.

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::api::{ApiError, ApiResult};
use crate::ha_source::{MIN_WRITE_SECS, store};

/// The longest range one history request may ask for, like `/api/prices`.
const MAX_HISTORY_SECS: i64 = 31 * 24 * 3600;
/// A history answer has about this many points at most; a longer range is
/// averaged into buckets.
const TARGET_POINTS: i64 = 3000;

/// `{"configured", "connected", "entities": [{entity_id, name, metric,
/// state, value, unit, last_updated}]}`. `value` is the last numeric value
/// (null when the last state was text such as "unavailable"); `state` is the
/// last state text; `last_updated` is unix seconds. Before the first
/// connection the values are the last stored ones. Without
/// `[home_assistant.source]` the list is empty and `configured` is false.
pub async fn entities(State(state): State<AppState>) -> ApiResult {
    Ok(Json(match state.ha_view() {
        Some(view) => view.to_json(),
        None => json!({"configured": false, "connected": false, "entities": []}),
    }))
}

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    entity_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    /// Bucket length in seconds to average over. Left out, it is chosen so
    /// the answer has at most about 3000 points.
    step: Option<String>,
}

fn parse_secs(name: &str, text: Option<&str>) -> Result<i64, ApiError> {
    text.and_then(|t| t.parse().ok())
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a unix time in seconds")))
}

/// The averaging step for a request: `None` means raw rows. A step at or
/// below the storage rate would not average anything, so it is raw too.
fn effective_step(range_secs: i64, requested: Option<i64>) -> Option<i64> {
    let step = requested.unwrap_or_else(|| (range_secs + TARGET_POINTS - 1) / TARGET_POINTS);
    (step > MIN_WRITE_SECS).then_some(step)
}

/// `{"entity_id", "unit", "step", "points": [{"ts", "value"}]}` for the
/// numeric readings with `from <= ts < to` (unix seconds, at most 31 days),
/// oldest first. Values are as Home Assistant reported them, in `unit`. With
/// `step` (seconds) or on a long range they are averages per bucket and `ts`
/// is the bucket start; `step` is null for raw rows.
pub async fn history(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> ApiResult {
    let entity_id = params
        .entity_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::bad_request("entity_id is required"))?;
    let from = parse_secs("from", params.from.as_deref())?;
    let to = parse_secs("to", params.to.as_deref())?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if to - from > MAX_HISTORY_SECS {
        return Err(ApiError::bad_request("the range can be at most 31 days"));
    }
    let requested = match params.step.as_deref() {
        None => None,
        Some(text) => Some(
            text.parse::<i64>()
                .ok()
                .filter(|step| *step > 0)
                .ok_or_else(|| ApiError::bad_request("step must be a whole number of seconds"))?,
        ),
    };
    let step = effective_step(to - from, requested);
    let rows = store::history(&state.pool, entity_id, from, to, step).await?;
    let unit = store::latest(&state.pool, entity_id)
        .await?
        .and_then(|(_, _, unit)| unit);
    let points: Vec<_> = rows
        .into_iter()
        .map(|(ts, value)| json!({"ts": ts, "value": value}))
        .collect();
    Ok(Json(json!({
        "entity_id": entity_id,
        "unit": unit,
        "step": step,
        "points": points,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_ranges_are_raw_and_long_ones_are_bucketed() {
        assert_eq!(effective_step(3600, None), None);
        // 12.5 hours is exactly 3000 rows at the 15 s rate: still raw.
        assert_eq!(effective_step(3000 * 15, None), None);
        assert_eq!(effective_step(31 * 24 * 3600, None), Some(893));
        assert_eq!(effective_step(3600, Some(300)), Some(300));
        assert_eq!(effective_step(3600, Some(10)), None);
    }
}
