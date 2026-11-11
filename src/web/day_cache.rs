//! Stored copies of finished days' figures (the `daily_cache` table), for the
//! pages that would otherwise re-read and re-process every 15-second sample of
//! the range on each load. `/api/costs` and `/api/battery` have their own
//! tables for the same reason; this one is shared by the Amber comparison and
//! what-if, the bill's tariff split, the price stats and the payback page, each
//! under its own `kind`.
//!
//! A stored day is only used while its key equals the key the day would get
//! now. Each caller builds the key from everything its figures depend on (a
//! version constant, the time zone, the plan, the price fingerprint and so on),
//! so a changed plan or a late price makes the day be worked out again. The
//! figures are stored as JSON, which gives back exactly the same numbers
//! (`serde_json` is built with `float_roundtrip`), so a stored day and a
//! freshly worked out one answer byte for byte the same.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::NaiveDate;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

use super::api::ApiError;
use crate::db::{self, DailyCacheRow};

/// The stored days of `kind` among `keys` (each date with the key it has now).
/// A day with no row, a row with another key, or a row that no longer reads
/// back is left out, and the caller works it out again.
pub(super) async fn load<T: DeserializeOwned>(
    pool: &SqlitePool,
    kind: &str,
    keys: &[(NaiveDate, String)],
) -> Result<HashMap<NaiveDate, T>, ApiError> {
    let (Some((first, _)), Some((last, _))) = (keys.first(), keys.last()) else {
        return Ok(HashMap::new());
    };
    let wanted: HashMap<NaiveDate, &str> = keys.iter().map(|(d, k)| (*d, k.as_str())).collect();
    let rows = db::daily_cache_in_range(pool, kind, &first.to_string(), &last.to_string()).await?;
    let mut found = HashMap::new();
    for row in rows {
        let Ok(date) = NaiveDate::from_str(&row.date) else {
            continue;
        };
        if wanted.get(&date) != Some(&row.cache_key.as_str()) {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&row.payload) {
            found.insert(date, value);
        }
    }
    Ok(found)
}

/// Stores finished days of `kind`, each with the key it was worked out under.
pub(super) async fn save<T: Serialize>(
    pool: &SqlitePool,
    kind: &str,
    days: &[(NaiveDate, String, &T)],
) -> Result<(), ApiError> {
    let rows = days
        .iter()
        .map(|(date, key, value)| {
            Ok(DailyCacheRow {
                date: date.to_string(),
                cache_key: key.clone(),
                payload: serde_json::to_string(value)?,
            })
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()
        .map_err(anyhow::Error::from)?;
    db::save_daily_cache(pool, kind, &rows, db::now()).await?;
    Ok(())
}
