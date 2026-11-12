//! `GET /api/prices/grid`: the heat strip of the History page. For each local
//! day, the time-weighted average buy price in each half hour of the clock.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{NaiveDate, Timelike};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use super::api::{ApiError, ApiResult, day_bounds_or_error, parse_date, today};
use super::day_cache;
use crate::amber::store;
use crate::cost::PriceSpan;

/// The longest range one request may ask for.
const MAX_GRID_DAYS: u64 = 400;

/// The source prefix of Amber prices (see `amber::store::source_name`).
const AMBER_PREFIX: &str = "amber:";

/// The `daily_cache` kind for a day's half-hour cells.
const GRID_KIND: &str = "price_grid";

/// Bump when the cells, or how they are worked out, change.
const GRID_VERSION: u32 = 1;

/// Half hours of a wall-clock day, 00:00 to 23:30.
pub const CELLS: usize = 48;

#[derive(Debug, Deserialize)]
pub struct GridParams {
    from: Option<String>,
    to: Option<String>,
}

/// One day's cells, as stored: `null` where no settled price lies in that half hour.
type Cells = Vec<Option<f64>>;

/// The time-weighted average price (cents) of each wall-clock half hour of one
/// local day `[day_start, day_end)`, from the settled rows in `prices` (sorted
/// by start, may hold other days too: only rows that start inside the day count).
///
/// Cells are the clock's half hours, 00:00 to 23:30, whatever the day's length.
/// On a 25 hour day the repeated hour is averaged into the same wall-clock cells
/// (by time, so each pass counts for its own length); on a 23 hour day the hour
/// that never happened has `None` cells. A cell no row touches is `None` too.
/// A row is split where it crosses a half hour and clipped to the day, so a
/// 30-minute row and six 5-minute rows give the same cell.
pub fn day_cells(tz: Tz, day_start: i64, day_end: i64, prices: &[PriceSpan]) -> Cells {
    let first = prices.partition_point(|p| p.start_ts < day_start);
    let last = prices.partition_point(|p| p.start_ts < day_end);
    // (sum of cents * seconds, seconds) per cell.
    let mut acc = [(0.0_f64, 0_i64); CELLS];
    for span in &prices[first..last] {
        let end = span.end_ts.min(day_end);
        let mut t = span.start_ts;
        while t < end {
            let Some(local) = chrono::DateTime::from_timestamp(t, 0).map(|u| u.with_timezone(&tz))
            else {
                break;
            };
            let secs_of_day = i64::from(local.num_seconds_from_midnight());
            let cell = (secs_of_day / 1800) as usize;
            // The clock's next half hour; clocks only change on one, so this is
            // the same distance on the timeline.
            let piece_end = (t + (1800 - secs_of_day % 1800)).min(end);
            let secs = piece_end - t;
            if let Some(slot) = acc.get_mut(cell) {
                slot.0 += span.cents * secs as f64;
                slot.1 += secs;
            }
            t = piece_end;
        }
    }
    acc.iter()
        .map(|&(sum, secs)| (secs > 0).then(|| sum / secs as f64))
        .collect()
}

/// `GET /api/prices/grid?from=YYYY-MM-DD&to=YYYY-MM-DD`: for each local day
/// (site time zone, both ends included, at most 400 days) 48 half-hour cells of
/// the time-weighted average buy (`general`) price in cents over the settled
/// 5-minute rows inside them, or `null` where there are none (see `day_cells`).
/// Forecasts never count; the interval under way counts today.
/// `{"configured":bool,"spike_cents":..,"days":[{"date":"2026-11-08","cells":[..48..]}]}`.
/// Every day in the range is returned; `configured` is false when there is no
/// price source and no price in the range. Finished days are kept in the `daily_cache` table.
pub async fn grid(State(state): State<AppState>, Query(params): Query<GridParams>) -> ApiResult {
    let from = parse_date("from", params.from.as_deref().unwrap_or(""))?;
    let to = parse_date("to", params.to.as_deref().unwrap_or(""))?;
    if to < from {
        return Err(ApiError::bad_request("to must not be before from"));
    }
    if (to - from).num_days() as u64 >= MAX_GRID_DAYS {
        return Err(ApiError::bad_request("the range can be at most 400 days"));
    }
    let tz = state.tz;
    let today_date = today(tz);
    let spike_cents = state.admin.get().price_spike_cents;
    let dates: Vec<NaiveDate> = from.iter_days().take_while(|d| *d <= to).collect();

    // Finished days stored for these prices are loaded in one call.
    let mut keys = Vec::new();
    for &date in dates.iter().filter(|d| **d < today_date) {
        let (start, end) = day_bounds_or_error(date, tz)?;
        let fingerprint =
            store::day_prices_fingerprint(&state.pool, AMBER_PREFIX, start, end, false).await?;
        keys.push((date, format!("{GRID_VERSION}|{tz}|prices:{fingerprint}")));
    }
    let mut stored: HashMap<NaiveDate, Cells> =
        day_cache::load(&state.pool, GRID_KIND, &keys).await?;
    let missing: Vec<NaiveDate> = dates
        .iter()
        .copied()
        .filter(|d| !stored.contains_key(d))
        .collect();

    let key_of: HashMap<NaiveDate, &String> = keys.iter().map(|(d, k)| (*d, k)).collect();
    let (today_start, _) = day_bounds_or_error(today_date, tz)?;
    let mut to_store: Vec<(NaiveDate, String, Cells)> = Vec::new();
    // One range read for each run of consecutive missing days.
    for run in missing.chunk_by(|a, b| a.succ_opt() == Some(*b)) {
        let (range_start, _) = day_bounds_or_error(run[0], tz)?;
        let (_, range_end) = day_bounds_or_error(run[run.len() - 1], tz)?;
        let prices = store::known_general_prices(
            &state.pool,
            AMBER_PREFIX,
            range_start,
            range_end,
            today_start,
        )
        .await?;
        for &date in run {
            let (start, end) = day_bounds_or_error(date, tz)?;
            let cells = day_cells(tz, start, end, &prices);
            if let Some(key) = key_of.get(&date) {
                to_store.push((date, (*key).clone(), cells.clone()));
            }
            stored.insert(date, cells);
        }
    }
    let rows: Vec<(NaiveDate, String, &Cells)> = to_store
        .iter()
        .map(|(date, key, cells)| (*date, key.clone(), cells))
        .collect();
    day_cache::save(&state.pool, GRID_KIND, &rows).await?;

    // A price source counts as configured when it is set up, or already has prices in the range.
    let mut configured = state.amber_configured;
    let days: Vec<_> = dates
        .iter()
        .map(|date| {
            let cells = stored.remove(date).unwrap_or_else(|| vec![None; CELLS]);
            configured |= cells.iter().any(Option::is_some);
            json!({"date": date.to_string(), "cells": cells})
        })
        .collect();
    Ok(Json(json!({
        "configured": configured,
        "spike_cents": spike_cents,
        "days": days,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const TZ: Tz = chrono_tz::Australia::Adelaide;

    fn ts(tz: Tz, text: &str) -> i64 {
        let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        tz.from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn span(start_ts: i64, secs: i64, cents: f64) -> PriceSpan {
        PriceSpan {
            start_ts,
            end_ts: start_ts + secs,
            cents,
        }
    }

    fn bounds(date: &str) -> (i64, i64) {
        day_bounds_or_error(parse_date("d", date).unwrap(), TZ).unwrap()
    }

    #[test]
    fn a_cell_is_the_time_weighted_average_of_its_rows() {
        let (start, end) = bounds("2026-11-05");
        let noon = ts(TZ, "2026-11-05 12:00");
        // 12:00 to 12:10 at 10c, 12:10 to 12:30 at 40c: (10*10 + 40*20) / 30 = 30c.
        let prices = [span(noon, 600, 10.0), span(noon + 600, 1200, 40.0)];
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells.len(), 48);
        assert!((cells[24].unwrap() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn cells_without_prices_are_null() {
        let (start, end) = bounds("2026-11-05");
        let prices = [span(ts(TZ, "2026-11-05 00:00"), 300, 7.5)];
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells[0], Some(7.5));
        assert!(cells[1..].iter().all(Option::is_none));
        assert!(day_cells(TZ, start, end, &[]).iter().all(Option::is_none));
    }

    #[test]
    fn a_row_is_split_where_it_crosses_a_half_hour_and_clipped_to_the_day() {
        let (start, end) = bounds("2026-11-05");
        // A 30-minute row starting at 12:15 (+1 s like Amber's) covers 15 min of two cells.
        let prices = [span(ts(TZ, "2026-11-05 12:15") + 1, 1800, 20.0)];
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells[24], Some(20.0));
        assert_eq!(cells[25], Some(20.0));
        assert!(cells[26].is_none());
        // A row running past midnight does not leak into the next day's first cell.
        let late = [span(end - 300, 900, 5.0)];
        let cells = day_cells(TZ, start, end, &late);
        assert_eq!(cells[47], Some(5.0));
        assert_eq!(cells.iter().flatten().count(), 1);
    }

    #[test]
    fn rows_of_other_days_do_not_count() {
        let (start, end) = bounds("2026-11-05");
        let prices = [
            span(start - 600, 300, 99.0),
            span(ts(TZ, "2026-11-05 01:00"), 300, 1.0),
            span(end + 600, 300, 99.0),
        ];
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells.iter().flatten().count(), 1);
        assert_eq!(cells[2], Some(1.0));
    }

    #[test]
    fn a_25_hour_day_averages_the_repeated_hour_into_its_wall_clock_cells() {
        // Adelaide's clocks go back on 5 Apr 2026: 03:00 ACDT becomes 02:00 ACST.
        let (start, end) = bounds("2026-04-05");
        assert_eq!(end - start, 25 * 3600);
        let first_pass = start + 2 * 3600; // 02:00 ACDT
        let second_pass = start + 3 * 3600; // 02:00 ACST
        let prices = [span(first_pass, 1800, 10.0), span(second_pass, 1800, 30.0)];
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells.len(), 48);
        assert_eq!(cells[4], Some(20.0));
        assert_eq!(cells.iter().flatten().count(), 1);
    }

    #[test]
    fn a_23_hour_day_has_null_cells_for_the_missing_hour() {
        // Clocks go forward on 4 Oct 2026: 02:00 ACST becomes 03:00 ACDT.
        let (start, end) = bounds("2026-10-04");
        assert_eq!(end - start, 23 * 3600);
        // A price for every 5 minutes of the whole day.
        let prices: Vec<PriceSpan> = (0..(end - start) / 300)
            .map(|i| span(start + i * 300, 300, 12.0))
            .collect();
        let cells = day_cells(TZ, start, end, &prices);
        assert_eq!(cells.len(), 48);
        assert_eq!(cells.iter().flatten().count(), 46);
        assert!(cells[4].is_none() && cells[5].is_none());
        assert_eq!(cells[3], Some(12.0));
        assert_eq!(cells[6], Some(12.0));
    }
}
