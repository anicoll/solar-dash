//! Warms the caches after startup and after each nightly GloBird sync.
//!
//! The first load of the bill, the payback page, the battery page and the
//! History ranges reads and works out a lot of days (up to a second each on a
//! big database); later loads come from the stored days and are quick. This
//! task makes those first calls itself, a few seconds after the server starts
//! and again when a sync has brought in new data, so the person's first visit
//! is already fast. It calls the handlers' own functions, so what it stores is
//! exactly what a visit would store. A failure is logged and ignored.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use chrono::Days;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::{debug, info};

use super::api::{self, ApiResult, today};
use super::{AppState, battery, bill, payback, whatif};

/// How long after startup the first warm-up waits, so it does not compete
/// with the sources' first sync for the database.
const STARTUP_DELAY: Duration = Duration::from_secs(5);

/// How long after a sync the warm-up waits, for its last writes to settle.
const SYNC_DELAY: Duration = Duration::from_secs(5);

/// Warms once after startup, then once after each completed GloBird sync.
/// Runs until the process ends.
pub async fn run(state: AppState) {
    tokio::time::sleep(STARTUP_DELAY).await;
    warm(&state).await;
    let synced = state.warm_signal();
    loop {
        synced.notified().await;
        tokio::time::sleep(SYNC_DELAY).await;
        warm(&state).await;
    }
}

/// Builds a handler's query parameters from JSON, so this does not need the
/// handlers' parameter structs to be public.
fn query<T: DeserializeOwned>(params: Value) -> Option<Query<T>> {
    serde_json::from_value(params).ok().map(Query)
}

/// Logs a failed call at debug level and gives back the answer, if any.
fn checked(name: &str, result: ApiResult) -> Option<Value> {
    match result {
        Ok(Json(value)) => Some(value),
        Err(err) => {
            debug!(call = name, "cache warm-up call failed: {err:?}");
            None
        }
    }
}

/// Makes the first-load calls once. Anything not configured answers
/// `{"configured": false}` quickly, so it costs nothing.
pub async fn warm(state: &AppState) {
    let started = Instant::now();
    let today = today(state.tz);
    let day = |n: u64| (today - Days::new(n)).to_string();
    let mut calls = 0;

    // The current bill period (and its dates, for the what-if below).
    let bill_now = match query(json!({})) {
        Some(q) => checked("bill/current", bill::current(State(state.clone()), q).await),
        None => None,
    };
    calls += 1;

    if let Some(q) = query(json!({ "days": "31" })) {
        checked("battery", battery::battery(State(state.clone()), q).await);
        calls += 1;
    }
    checked("payback", payback::payback(State(state.clone())).await);
    calls += 1;

    for params in [
        json!({ "from": day(30), "to": today.to_string() }),
        json!({ "from": day(364), "to": today.to_string(), "group": "month" }),
    ] {
        if let Some(q) = query(params) {
            checked("daily", api::daily(State(state.clone()), q).await);
            calls += 1;
        }
    }

    // The what-if only makes sense with `[amber]`.
    if state.amber_compare.is_some() {
        let period = bill_now.as_ref().and_then(|b| {
            let text = |key| b.get(key).and_then(Value::as_str).map(str::to_owned);
            Some((text("from")?, text("to")?))
        });
        let mut ranges = vec![(day(29), today.to_string())];
        ranges.extend(period);
        for (from, to) in ranges {
            if let Some(q) = query(json!({ "from": from, "to": to })) {
                checked(
                    "amber-whatif",
                    whatif::amber_whatif(State(state.clone()), q).await,
                );
                calls += 1;
            }
        }
    }
    info!(
        calls,
        took_ms = started.elapsed().as_millis() as u64,
        "cache warm-up done"
    );
}
