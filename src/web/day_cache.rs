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
use chrono_tz::Tz;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

use super::api::{ApiError, day_bounds_or_error};
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

/// What a finished day's key gets when it is stored without all its data (a
/// gap the inverter or the sync missed): the key it would have, then this tag
/// and a fingerprint of the day's stored readings (see
/// [`crate::db::data_fingerprints`]). A later backfill changes the
/// fingerprint, so the day is worked out again, while a day nobody touches
/// is read back for good. Days with all their data keep the plain key.
const DATA_TAG: &str = "|data:";

/// `key` as an incomplete day is stored under, given the day's fingerprint.
pub(super) fn data_key(key: &str, fingerprint: &str) -> String {
    format!("{key}{DATA_TAG}{fingerprint}")
}

/// Whether a stored `row_key` is an incomplete-day key built on `key`.
pub(super) fn is_data_key_of(row_key: &str, key: &str) -> bool {
    row_key
        .strip_prefix(key)
        .is_some_and(|rest| rest.starts_with(DATA_TAG))
}

/// The fingerprint of each of `dates`' stored readings, as [`data_key`] takes it.
pub(super) async fn fingerprints(
    pool: &SqlitePool,
    plant_id: i64,
    tz: Tz,
    dates: &[NaiveDate],
) -> Result<HashMap<NaiveDate, String>, ApiError> {
    let mut sorted = dates.to_vec();
    sorted.sort();
    sorted.dedup();
    let bounds = sorted
        .iter()
        .map(|d| day_bounds_or_error(*d, tz))
        .collect::<Result<Vec<_>, _>>()?;
    let prints = db::data_fingerprints(pool, plant_id, &bounds).await?;
    Ok(sorted.into_iter().zip(prints).collect())
}

/// [`load`], and a day stored as incomplete is found too when the readings it
/// was worked out from are still the same. `keys` hold each day's plain key.
pub(super) async fn load_with_data<T: DeserializeOwned>(
    pool: &SqlitePool,
    plant_id: i64,
    tz: Tz,
    kind: &str,
    keys: &[(NaiveDate, String)],
) -> Result<HashMap<NaiveDate, T>, ApiError> {
    let (Some((first, _)), Some((last, _))) = (keys.first(), keys.last()) else {
        return Ok(HashMap::new());
    };
    let wanted: HashMap<NaiveDate, &str> = keys.iter().map(|(d, k)| (*d, k.as_str())).collect();
    let rows = db::daily_cache_in_range(pool, kind, &first.to_string(), &last.to_string()).await?;
    let rows: Vec<(NaiveDate, DailyCacheRow)> = rows
        .into_iter()
        .filter_map(|row| Some((NaiveDate::from_str(&row.date).ok()?, row)))
        .filter(|(date, _)| wanted.contains_key(date))
        .collect();
    // Only the days stored as incomplete need their readings fingerprinted.
    let incomplete: Vec<NaiveDate> = rows
        .iter()
        .filter(|(date, row)| is_data_key_of(&row.cache_key, wanted[date]))
        .map(|(date, _)| *date)
        .collect();
    let prints = fingerprints(pool, plant_id, tz, &incomplete).await?;
    let mut found = HashMap::new();
    for (date, row) in rows {
        let key = wanted[&date];
        let current = match prints.get(&date) {
            Some(print) => data_key(key, print),
            None => key.to_string(),
        };
        if row.cache_key != current {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&row.payload) {
            found.insert(date, value);
        }
    }
    Ok(found)
}

/// The key to store each finished day under: its plain key when its data is
/// complete (the `bool`), else the plain key with the fingerprint of its
/// readings (see [`data_key`]). Costs one grouped query for all the
/// incomplete days, and none when every day is complete.
pub(super) async fn store_keys(
    pool: &SqlitePool,
    plant_id: i64,
    tz: Tz,
    days: &[(NaiveDate, String, bool)],
) -> Result<Vec<(NaiveDate, String)>, ApiError> {
    let incomplete: Vec<NaiveDate> = days
        .iter()
        .filter(|(_, _, complete)| !complete)
        .map(|(date, _, _)| *date)
        .collect();
    let prints = fingerprints(pool, plant_id, tz, &incomplete).await?;
    Ok(days
        .iter()
        .map(|(date, key, _)| match prints.get(date) {
            Some(print) => (*date, data_key(key, print)),
            None => (*date, key.clone()),
        })
        .collect())
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
