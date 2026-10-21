//! `/api/costs`: what each local day cost, and what it would have cost
//! without solar, using the `[tariff]` from the config.
//!
//! This file turns stored data into the [`Interval`]s that `cost::day_cost`
//! prices. The interval builders are pure, so they are tested with plain numbers.

use std::collections::HashMap;
use std::str::FromStr;

use axum::Json;
use axum::extract::{Query, State};
use chrono::NaiveDate;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::api::{
    ApiError, ApiResult, DEFAULT_DAILY_DAYS, MAX_HOLD_SECS, Spread, date_range,
    day_bounds_or_error, live_totals, spread_steps, today,
};
use crate::cost::{self, DayCost, Interval, Tariff, TariffSchedule};
use crate::db;
use crate::modbus::Sample;

/// The whole history since Feb 2023 is about 1,000 days; this leaves room to grow.
const MAX_COSTS_DAYS: u64 = 1500;
/// Live samples are loaded this many days at a time. A year of them is
/// millions of rows, too many to hold at once on a small machine.
const LIVE_CHUNK_DAYS: usize = 31;

/// The cloud counters (Wh, reset at local midnight) the cloud cost comes from.
const IMPORT_COUNTER: i64 = 83102;
const EXPORT_COUNTER: i64 = 83072;
const LOAD_COUNTER: i64 = 83118;

/// W·s in one kWh.
const WS_PER_KWH: f64 = 3.6e6;

/// Intervals from live samples. Each sample is held until the next one, for
/// at most [`MAX_HOLD_SECS`] (as `integrate` does), so a dongle dropout adds
/// no energy. The last sample has no next sample and adds nothing. A missing
/// value counts as 0. Grid power splits into import (positive) and export
/// (negative).
fn live_intervals(samples: &[Sample]) -> Vec<Interval> {
    samples
        .windows(2)
        .map(|pair| {
            let secs = (pair[1].ts - pair[0].ts).min(MAX_HOLD_SECS);
            let to_kwh = |watts: f64| watts * secs as f64 / WS_PER_KWH;
            let grid_w = pair[0].grid_w.unwrap_or(0.0);
            Interval {
                start_ts: pair[0].ts,
                end_ts: pair[0].ts + secs,
                import_kwh: to_kwh(grid_w.max(0.0)),
                export_kwh: to_kwh((-grid_w).max(0.0)),
                load_kwh: to_kwh(pair[0].load_w.unwrap_or(0.0)),
            }
        })
        .collect()
}

/// A day's counter readings with a Wh = 0 reading added at local midnight.
/// The counter resets then, so whatever the first reading shows was earned
/// since midnight. Without this the first 5 minutes of energy would be lost.
fn with_midnight_zero(day_start: i64, counter: &[(i64, f64)]) -> Vec<(i64, f64)> {
    let mut points = Vec::with_capacity(counter.len() + 1);
    // A reading exactly at midnight is already the start (and two readings at
    // one time would divide by zero in `spread_steps`).
    if counter.first().is_some_and(|&(ts, _)| ts > day_start) {
        points.push((day_start, 0.0));
    }
    points.extend_from_slice(counter);
    points
}

/// kWh of `spreads` (sorted, not overlapping) that fall within `[from, to)`.
/// `skip` is how many spreads were already passed; it moves forward, so
/// walking the day in order never looks back.
fn spread_kwh_between(spreads: &[Spread], skip: &mut usize, from: i64, to: i64) -> f64 {
    while *skip < spreads.len() && spreads[*skip].end <= from {
        *skip += 1;
    }
    spreads[*skip..]
        .iter()
        .take_while(|s| s.start < to)
        .map(|s| {
            let overlap = to.min(s.end) - from.max(s.start);
            s.wh / 1000.0 * overlap as f64 / (s.end - s.start) as f64
        })
        .sum()
}

/// Intervals from one day's cloud counters (`(ts, Wh)`, oldest first, from
/// `day_start`). Each counter's steps are spread over time like `derive_power`
/// does (see `spread_steps`). The three are then cut at every reading time and
/// spread start, so an interval is short and can be priced by its own time of
/// day. A counter with no readings counts as 0.
fn cloud_intervals(
    day_start: i64,
    import: &[(i64, f64)],
    export: &[(i64, f64)],
    load: &[(i64, f64)],
) -> Vec<Interval> {
    let counters = [import, export, load].map(|c| with_midnight_zero(day_start, c));
    let spreads = counters.clone().map(|c| spread_steps(&c));

    // Every time any counter was read, and every time a spread starts (a long
    // still spell ends 30 minutes before the step), oldest first, each once.
    let mut times: Vec<i64> = counters.iter().flatten().map(|&(ts, _)| ts).collect();
    times.extend(spreads.iter().flatten().map(|s| s.start));
    times.sort_unstable();
    times.dedup();

    let mut skips = [0; 3];
    times
        .windows(2)
        .map(|pair| {
            let mut kwh = [0.0; 3];
            for i in 0..3 {
                kwh[i] = spread_kwh_between(&spreads[i], &mut skips[i], pair[0], pair[1]);
            }
            Interval {
                start_ts: pair[0],
                end_ts: pair[1],
                import_kwh: kwh[0],
                export_kwh: kwh[1],
                load_kwh: kwh[2],
            }
        })
        .collect()
}

/// Everything stored for one local day `[start, end)`.
struct DayData<'a> {
    start: i64,
    end: i64,
    live: &'a [Sample],
    import: &'a [(i64, f64)],
    export: &'a [(i64, f64)],
    load: &'a [(i64, f64)],
}

/// The intervals of one day and where they came from, or `None` if there is
/// no data. Live samples win when they cover the day, the same test as
/// `/api/daily` uses; otherwise the cloud counters are used.
fn day_intervals(day: &DayData, now: i64) -> Option<(&'static str, Vec<Interval>)> {
    if live_totals(day.live, day.start, day.end, now).is_some() {
        return Some(("inverter", live_intervals(day.live)));
    }
    if day.import.is_empty() && day.export.is_empty() && day.load.is_empty() {
        return None;
    }
    let intervals = cloud_intervals(day.start, day.import, day.export, day.load);
    Some(("cloud", intervals))
}

/// The rows of a list sorted by time that fall in `[start, end)`.
fn slice_between<T>(rows: &[T], ts_of: impl Fn(&T) -> i64, start: i64, end: i64) -> &[T] {
    let first = rows.partition_point(|row| ts_of(row) < start);
    let last = rows.partition_point(|row| ts_of(row) < end);
    &rows[first..last]
}

/// Cents to dollars, rounded to 2 decimals.
fn dollars(cents: f64) -> f64 {
    round2(cents / 100.0)
}

pub(super) fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[derive(Debug, Serialize)]
struct CostDay {
    date: String,
    /// "inverter" (live samples) or "cloud", as in `/api/daily`.
    source: &'static str,
    actual: f64,
    baseline: f64,
    savings: f64,
    import_kwh: f64,
    export_kwh: f64,
    load_kwh: f64,
    /// Dollars of zero-import credit earned this day.
    credit: f64,
}

/// One priced local day, whether it came from the cache or was just worked out.
#[derive(Debug, Clone)]
pub(super) struct CostedDay {
    pub date: NaiveDate,
    /// "inverter" or "cloud".
    pub source: &'static str,
    pub cost: DayCost,
}

/// The `source` text of a stored row, or `None` for text we do not know (then
/// the row is ignored and the day is worked out again).
fn known_source(text: &str) -> Option<&'static str> {
    match text {
        "inverter" => Some("inverter"),
        "cloud" => Some("cloud"),
        _ => None,
    }
}

/// What stored costs are tagged with. The time zone is in it too, because
/// moving it moves where every local day starts.
fn cache_key(tz: Tz, tariff: &Tariff) -> String {
    format!("{tz}|{}", tariff.key())
}

/// May this day's cost be stored for good? Only a finished day with all its
/// data: it is before `today`, and either it used the inverter (which already
/// needs good coverage) or the cloud counters were read in the day's last 10
/// minutes (23:50 local or later). `newest_counter_ts` is the latest cloud
/// reading in the day. A day that fails this is worked out again next time, so
/// a backfill that fills it in later is picked up.
fn is_cacheable(
    date: NaiveDate,
    today: NaiveDate,
    source: &str,
    newest_counter_ts: Option<i64>,
    day_end: i64,
) -> bool {
    if date >= today {
        return false;
    }
    // Ten minutes before the end is 23:50 local, even on a daylight saving change.
    source == "inverter" || newest_counter_ts.is_some_and(|ts| ts >= day_end - 10 * 60)
}

/// Splits sorted dates into runs of consecutive days, so each run is loaded
/// with one set of queries instead of one set per day.
fn consecutive_runs(dates: &[NaiveDate]) -> Vec<&[NaiveDate]> {
    dates.chunk_by(|a, b| a.succ_opt() == Some(*b)).collect()
}

/// Works out the costs of consecutive local `dates` from the stored readings.
/// Days with no data are left out. Each day comes with whether it may be cached.
async fn compute_days(
    state: &AppState,
    tariff: &Tariff,
    dates: &[NaiveDate],
    today: NaiveDate,
) -> Result<Vec<(CostedDay, bool)>, ApiError> {
    let mut bounds = Vec::with_capacity(dates.len());
    for &date in dates {
        bounds.push(day_bounds_or_error(date, state.tz)?);
    }
    let range_start = bounds[0].0;
    let range_end = bounds[bounds.len() - 1].1;

    // One query for all three counters over the whole range, then split.
    let rows = db::readings_for_points(
        &state.pool,
        state.plant_id,
        &[IMPORT_COUNTER, EXPORT_COUNTER, LOAD_COUNTER],
        range_start,
        range_end,
    )
    .await?;
    let counter = |point_id: i64| -> Vec<(i64, f64)> {
        rows.iter()
            .filter(|row| row.0 == point_id)
            .map(|row| (row.1, row.2))
            .collect()
    };
    let (import, export, load) = (
        counter(IMPORT_COUNTER),
        counter(EXPORT_COUNTER),
        counter(LOAD_COUNTER),
    );

    let live_range = db::live_ts_range(&state.pool).await?;
    let now = db::now();
    let mut days = Vec::new();

    for (chunk_dates, chunk_bounds) in dates
        .chunks(LIVE_CHUNK_DAYS)
        .zip(bounds.chunks(LIVE_CHUNK_DAYS))
    {
        // Only the part of the chunk the live samples cover is loaded.
        let chunk_start = chunk_bounds[0].0;
        let chunk_end = chunk_bounds[chunk_bounds.len() - 1].1;
        let live = match live_range {
            Some((first, last)) if first < chunk_end && last >= chunk_start => {
                let from = chunk_start.max(first);
                let to = chunk_end.min(last + 1);
                db::live_samples_raw(&state.pool, from, to).await?
            }
            _ => Vec::new(),
        };

        for (&date, &(start, end)) in chunk_dates.iter().zip(chunk_bounds) {
            let day = DayData {
                start,
                end,
                live: slice_between(&live, |s| s.ts, start, end),
                import: slice_between(&import, |r| r.0, start, end),
                export: slice_between(&export, |r| r.0, start, end),
                load: slice_between(&load, |r| r.0, start, end),
            };
            let Some((source, intervals)) = day_intervals(&day, now) else {
                continue;
            };
            let newest_counter_ts = [day.import, day.export, day.load]
                .iter()
                .filter_map(|c| c.last().map(|r| r.0))
                .max();
            let cacheable = is_cacheable(date, today, source, newest_counter_ts, end);
            let cost = cost::day_cost(tariff, state.tz, &intervals);
            days.push((CostedDay { date, source, cost }, cacheable));
        }
    }
    Ok(days)
}

/// The cost of every day in `dates` (sorted, consecutive) that has data,
/// oldest first, each priced on the plan in effect that day. Runs of days on
/// the same plan are costed together by [`costed_days_on`].
pub(super) async fn costed_days(
    state: &AppState,
    schedule: &TariffSchedule,
    dates: &[NaiveDate],
) -> Result<Vec<CostedDay>, ApiError> {
    let mut costed = Vec::new();
    // Plans are in date order, so the days on one plan are next to each other.
    for run in dates.chunk_by(|a, b| std::ptr::eq(schedule.for_date(*a), schedule.for_date(*b))) {
        costed.extend(costed_days_on(state, schedule.for_date(run[0]), run).await?);
    }
    Ok(costed)
}

/// The cost of every day in `dates` (sorted, consecutive) that has data, all
/// on `tariff`. Finished days come from the `daily_cost` table when it has
/// them for this tariff; only the rest are worked out, in a few queries, and
/// the finished ones are stored for next time.
async fn costed_days_on(
    state: &AppState,
    tariff: &Tariff,
    dates: &[NaiveDate],
) -> Result<Vec<CostedDay>, ApiError> {
    let (Some(first), Some(last)) = (dates.first(), dates.last()) else {
        return Ok(Vec::new());
    };
    let key = cache_key(state.tz, tariff);
    let stored =
        db::daily_costs_in_range(&state.pool, &first.to_string(), &last.to_string(), &key).await?;
    let mut by_date: HashMap<String, CostedDay> = HashMap::new();
    for row in stored {
        let parsed = (NaiveDate::from_str(&row.date), known_source(&row.source));
        if let (Ok(date), Some(source)) = parsed {
            by_date.insert(
                row.date,
                CostedDay {
                    date,
                    source,
                    cost: row.cost,
                },
            );
        }
    }

    let missing: Vec<NaiveDate> = dates
        .iter()
        .copied()
        .filter(|date| !by_date.contains_key(&date.to_string()))
        .collect();
    let today = today(state.tz);
    let mut to_store = Vec::new();
    for run in consecutive_runs(&missing) {
        for (day, cacheable) in compute_days(state, tariff, run, today).await? {
            if cacheable {
                to_store.push(db::CachedDayCost {
                    date: day.date.to_string(),
                    source: day.source.to_string(),
                    cost: day.cost.clone(),
                });
            }
            by_date.insert(day.date.to_string(), day);
        }
    }
    db::save_daily_costs(&state.pool, &to_store, &key).await?;

    // Back in date order, which `HashMap` does not keep.
    Ok(dates
        .iter()
        .filter_map(|date| by_date.remove(&date.to_string()))
        .collect())
}

#[derive(Deserialize)]
pub struct CostsParams {
    from: Option<String>,
    to: Option<String>,
}

pub async fn costs(State(state): State<AppState>, Query(params): Query<CostsParams>) -> ApiResult {
    let (from, count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        MAX_COSTS_DAYS,
        DEFAULT_DAILY_DAYS,
    )?;
    let Some(tariff) = &state.tariff else {
        return Ok(Json(json!({ "configured": false })));
    };

    let dates: Vec<NaiveDate> = from.iter_days().take(count as usize).collect();
    let costed = costed_days(&state, tariff, &dates).await?;

    let (mut actual, mut baseline, mut credit_days) = (0.0, 0.0, 0);
    let mut days = Vec::with_capacity(costed.len());
    for CostedDay { date, source, cost } in costed {
        actual += cost.actual_cents;
        baseline += cost.baseline_cents;
        if cost.zero_import_credit_cents > 0.0 {
            credit_days += 1;
        }
        days.push(CostDay {
            date: date.to_string(),
            source,
            actual: dollars(cost.actual_cents),
            baseline: dollars(cost.baseline_cents),
            savings: dollars(cost.savings_cents),
            import_kwh: round2(cost.import_kwh),
            export_kwh: round2(cost.export_kwh),
            load_kwh: round2(cost.load_kwh),
            credit: dollars(cost.zero_import_credit_cents),
        });
    }

    // Totals are added up from the unrounded cents, so they do not drift from
    // the true sum by a cent per day.
    Ok(Json(json!({
        "configured": true,
        "totals": {
            "actual": dollars(actual),
            "baseline": dollars(baseline),
            "savings": dollars(baseline - actual),
            "credit_days": credit_days,
            "days": days.len(),
        },
        "days": days,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, load_w: Option<f64>, grid_w: Option<f64>) -> Sample {
        Sample {
            ts,
            pv_w: None,
            load_w,
            grid_w,
            battery_w: None,
            soc_pct: None,
        }
    }

    fn total(intervals: &[Interval], get: fn(&Interval) -> f64) -> f64 {
        intervals.iter().map(get).sum()
    }

    #[test]
    fn live_samples_become_held_intervals() {
        let samples = [
            sample(0, Some(500.0), Some(1000.0)),  // import 1 kW, held 60 s
            sample(60, None, Some(-2000.0)),       // export 2 kW, held 60 s; load missing
            sample(120, Some(100.0), None),        // grid missing: 0
            sample(1000, Some(100.0), Some(10.0)), // gap: previous held only 120 s
            sample(1015, Some(0.0), Some(0.0)),
        ];
        let intervals = live_intervals(&samples);
        assert_eq!(intervals.len(), 4);
        assert_eq!((intervals[0].start_ts, intervals[0].end_ts), (0, 60));
        // The long gap is capped, not stretched to the next sample.
        assert_eq!((intervals[2].start_ts, intervals[2].end_ts), (120, 240));

        // 1 kW for 60 s, 2 kW for 60 s.
        assert!(
            (total(&intervals, |i| i.import_kwh) - (1000.0 * 60.0 + 10.0 * 15.0) / 3.6e6).abs()
                < 1e-12
        );
        assert!((total(&intervals, |i| i.export_kwh) - 2000.0 * 60.0 / 3.6e6).abs() < 1e-12);
        let load_ws = 500.0 * 60.0 + 100.0 * 120.0 + 100.0 * 15.0;
        assert!((total(&intervals, |i| i.load_kwh) - load_ws / 3.6e6).abs() < 1e-12);
        assert!(live_intervals(&[]).is_empty());
        assert!(live_intervals(&[sample(0, None, None)]).is_empty());
    }

    #[test]
    fn cloud_counters_become_intervals_that_conserve_energy() {
        let start = 1_000_000;
        // Import: 100 Wh at 00:15 after being still, 200 Wh more at 00:30.
        let import = [
            (start + 300, 0.0),
            (start + 600, 0.0),
            (start + 900, 100.0),
            (start + 1200, 100.0),
            (start + 1500, 100.0),
            (start + 1800, 300.0),
        ];
        // Export has its own readings (offset from import's) and a gap.
        let export = [(start + 450, 0.0), (start + 1050, 100.0)];
        // Load already shows 400 Wh at the first reading: earned since midnight.
        let load = [(start + 300, 400.0), (start + 600, 500.0)];

        let intervals = cloud_intervals(start, &import, &export, &load);
        assert!((total(&intervals, |i| i.import_kwh) - 0.3).abs() < 1e-12);
        assert!((total(&intervals, |i| i.export_kwh) - 0.1).abs() < 1e-12);
        assert!((total(&intervals, |i| i.load_kwh) - 0.5).abs() < 1e-12);

        // Contiguous, in order, and each one shorter than the reading spacing.
        assert_eq!(intervals[0].start_ts, start);
        for pair in intervals.windows(2) {
            assert_eq!(pair[0].end_ts, pair[1].start_ts);
        }
        assert!(intervals.iter().all(|i| i.end_ts - i.start_ts <= 300));
        // The counter sat at 0 from midnight until 00:15, so its first 100 Wh
        // is spread over those 15 minutes: a third in each 5-minute interval.
        let at = |ts: i64| intervals.iter().find(|i| i.start_ts == ts).unwrap();
        assert!((at(start + 600).import_kwh - 0.1 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_counter_step_after_midnight_is_capped_at_half_an_hour() {
        let start = 0;
        // First reading at 03:00 already shows 1 kWh: spread over 02:30-03:00 only.
        let intervals = cloud_intervals(start, &[(3 * 3600, 1000.0)], &[], &[]);
        assert!((total(&intervals, |i| i.import_kwh) - 1.0).abs() < 1e-12);
        let first_energy = intervals.iter().find(|i| i.import_kwh > 0.0).unwrap();
        assert_eq!(first_energy.start_ts, 3 * 3600 - 1800);
        // A reading exactly at midnight is not duplicated.
        let intervals = cloud_intervals(start, &[(0, 0.0), (300, 50.0)], &[], &[]);
        assert_eq!(intervals.len(), 1);
        assert!((intervals[0].import_kwh - 0.05).abs() < 1e-12);
        assert!(cloud_intervals(start, &[], &[], &[]).is_empty());
    }

    #[test]
    fn days_without_data_have_no_intervals() {
        let empty = DayData {
            start: 0,
            end: 86_400,
            live: &[],
            import: &[],
            export: &[],
            load: &[],
        };
        assert!(day_intervals(&empty, 100_000).is_none());
        let some = DayData {
            load: &[(300, 10.0)],
            ..empty
        };
        let (source, intervals) = day_intervals(&some, 100_000).unwrap();
        assert_eq!(source, "cloud");
        assert_eq!(intervals.len(), 1);
    }

    #[test]
    fn money_is_rounded_to_cents() {
        assert_eq!(dollars(12345.0), 123.45);
        assert_eq!(dollars(-19.9), -0.2);
        assert_eq!(round2(1.005_f64 + 0.0004), 1.01);
    }

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    #[test]
    fn only_finished_complete_days_are_cached() {
        let (today, end) = (date("2026-09-16"), 100_000);
        let yesterday = date("2026-09-15");
        // The cloud counters must be read in the last 10 minutes of the day.
        assert!(is_cacheable(
            yesterday,
            today,
            "cloud",
            Some(end - 600),
            end
        ));
        assert!(!is_cacheable(
            yesterday,
            today,
            "cloud",
            Some(end - 601),
            end
        ));
        assert!(!is_cacheable(yesterday, today, "cloud", None, end));
        // Inverter days already passed the coverage test.
        assert!(is_cacheable(yesterday, today, "inverter", None, end));
        // Today is never cached, nor is the future.
        assert!(!is_cacheable(today, today, "inverter", Some(end), end));
        assert!(!is_cacheable(
            date("2026-09-17"),
            today,
            "cloud",
            Some(end),
            end
        ));
    }

    #[test]
    fn missing_days_group_into_consecutive_runs() {
        let dates = [
            "2026-09-01",
            "2026-09-02",
            "2026-09-05",
            "2026-09-07",
            "2026-09-08",
        ]
        .map(date);
        let runs = consecutive_runs(&dates);
        let lengths: Vec<usize> = runs.iter().map(|run| run.len()).collect();
        assert_eq!(lengths, vec![2, 1, 2]);
        assert!(consecutive_runs(&[]).is_empty());
    }

    #[test]
    fn the_cache_key_follows_the_zone_and_rates() {
        let make = |cents: &str| {
            let text = format!("supply_cents_per_day = 1.0\ndefault_import_cents = {cents}\n");
            Tariff::from_config(&toml::from_str(&text).unwrap()).unwrap()
        };
        let adelaide = chrono_tz::Australia::Adelaide;
        let key = cache_key(adelaide, &make("30.0"));
        assert_eq!(key, cache_key(adelaide, &make("30.0")));
        assert_ne!(key, cache_key(adelaide, &make("31.0")));
        assert_ne!(key, cache_key(chrono_tz::Australia::Perth, &make("30.0")));
    }
}
