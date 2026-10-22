//! `/api/battery`: how well the battery and the evening credit are working.
//!
//! For each recent local day it reports whether the zero-import credit was
//! earned, how much was exported and imported in the credit window, the
//! battery's charge at a few times of day, what was bought in the morning
//! before free power starts, and, for missed days, a best guess at why.
//!
//! The day-level maths is in small pure functions, tested with plain numbers.
//! The handler at the bottom only loads data and joins it up.

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::{Query, State};
use chrono::{Days, LocalResult, NaiveDate, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::api::{ApiError, ApiResult, SOC_POINT, day_bounds_or_error, display_value, today};
use super::costs::{
    DayData, EXPORT_COUNTER, IMPORT_COUNTER, LOAD_COUNTER, day_intervals, round2, slice_between,
};
use crate::cost::{self, Interval, Tariff, local_minute_of_day};
use crate::db::{self, ChargeSession, GlobirdCostRow};
use crate::ev_cost;
use crate::globird::data::{Bucket, bucket_of, is_supply};
use crate::modbus::Sample;

const DEFAULT_DAYS: u64 = 30;
const MAX_DAYS: u64 = 92;
/// A battery reading further than this from the wanted time does not count.
const SOC_MAX_GAP_SECS: i64 = 15 * 60;
/// "Battery ran low" means the charge got this close (percentage points) to the floor.
const LOW_MARGIN_PCT: f64 = 3.0;
/// More load than this for a 5-minute spell is more than the battery can cover.
const HIGH_LOAD_KW: f64 = 5.0;
/// The start of the evening peak, when we look at the battery for a third time.
const EVENING_PEAK_MINUTE: u32 = 16 * 60;
/// Used when the plan has no free-import window.
const DEFAULT_FREE_START_MINUTE: u32 = 11 * 60;
/// Load is checked in spells this long.
const LOAD_SPELL_SECS: i64 = 5 * 60;
/// A spell with less data than this is ignored (one stray sample proves nothing).
const MIN_SPELL_SECS: i64 = 60;

// ---------------------------------------------------------------------------
// Battery charge at a time of day
// ---------------------------------------------------------------------------

/// A day's battery charge readings (`(unix time, percent)`, oldest first):
/// the live ones from the inverter, and the ones from iSolarCloud. The live
/// ones are tried first for every question; the cloud ones fill the gaps.
#[derive(Debug, Default)]
struct SocSeries {
    live: Vec<(i64, f64)>,
    cloud: Vec<(i64, f64)>,
}

/// The reading closest to `ts`, if one is within [`SOC_MAX_GAP_SECS`].
fn nearest_soc(points: &[(i64, f64)], ts: i64) -> Option<f64> {
    points
        .iter()
        .min_by_key(|(t, _)| (t - ts).abs())
        .filter(|(t, _)| (t - ts).abs() <= SOC_MAX_GAP_SECS)
        .map(|&(_, soc)| soc)
}

/// The lowest reading with `from <= ts <= to`.
fn lowest_soc(points: &[(i64, f64)], from: i64, to: i64) -> Option<f64> {
    points
        .iter()
        .filter(|(t, _)| (from..=to).contains(t))
        .map(|&(_, soc)| soc)
        .min_by(f64::total_cmp)
}

impl SocSeries {
    fn at(&self, ts: i64) -> Option<f64> {
        nearest_soc(&self.live, ts).or_else(|| nearest_soc(&self.cloud, ts))
    }

    fn lowest_between(&self, from: i64, to: i64) -> Option<f64> {
        lowest_soc(&self.live, from, to).or_else(|| lowest_soc(&self.cloud, from, to))
    }
}

// ---------------------------------------------------------------------------
// Energy sums for one day
// ---------------------------------------------------------------------------

/// What the intervals of one day add up to. Each interval is placed by the
/// local time of its midpoint, the same way `cost::day_cost` does.
#[derive(Debug, Default, PartialEq)]
struct DaySums {
    /// Whether any interval fell inside the credit window.
    has_window_data: bool,
    window_import_kwh: f64,
    window_export_kwh: f64,
    /// What the window's export earns at the tariff's feed-in price.
    window_export_cents: f64,
    /// The highest average load of any spell in the window, if there was data.
    peak_window_load_kw: Option<f64>,
    /// Import before free power starts, and what it cost.
    morning_import_kwh: f64,
    morning_import_cents: f64,
}

/// The highest average load (kW) over any 5-minute spell. `spells` holds, per
/// spell, the load's kWh and the seconds of data behind it.
fn peak_load_kw(spells: &BTreeMap<i64, (f64, i64)>) -> Option<f64> {
    spells
        .values()
        .filter(|(_, secs)| *secs >= MIN_SPELL_SECS)
        .map(|(kwh, secs)| kwh * 3600.0 / *secs as f64)
        .max_by(f64::total_cmp)
}

/// Adds up one day's intervals. `window` is the credit window and
/// `free_start` the minute free import begins (both minutes since midnight).
fn day_sums(tariff: &Tariff, tz: Tz, free_start: u32, intervals: &[Interval]) -> DaySums {
    let mut sums = DaySums::default();
    let mut spells: BTreeMap<i64, (f64, i64)> = BTreeMap::new();
    for iv in intervals {
        let mid = iv.start_ts + (iv.end_ts - iv.start_ts) / 2;
        let minute = local_minute_of_day(tz, mid);
        if tariff.in_credit_window(minute) {
            sums.has_window_data = true;
            sums.window_import_kwh += iv.import_kwh;
            sums.window_export_kwh += iv.export_kwh;
            sums.window_export_cents += iv.export_kwh * tariff.export_cents_at(minute);
            let spell = spells.entry(iv.start_ts / LOAD_SPELL_SECS).or_default();
            spell.0 += iv.load_kwh;
            spell.1 += iv.end_ts - iv.start_ts;
        }
        if minute < free_start {
            sums.morning_import_kwh += iv.import_kwh;
            sums.morning_import_cents += iv.import_kwh * tariff.import_cents_at(minute);
        }
    }
    sums.peak_window_load_kw = peak_load_kw(&spells);
    sums
}

// ---------------------------------------------------------------------------
// Credit and reason
// ---------------------------------------------------------------------------

/// Whether GloBird's bill earned the credit, for each day it has billed
/// completely (keyed by `YYYY-MM-DD`). A day with only its SUPPLY line is
/// still being billed and is left out. The credit line is negative when earned.
fn billed_credit_by_date(rows: &[GlobirdCostRow]) -> HashMap<String, bool> {
    let mut by_date = HashMap::new();
    for lines in rows.chunk_by(|a, b| a.date == b.date) {
        if lines.iter().all(|line| is_supply(&line.category)) {
            continue;
        }
        let credit: i64 = lines
            .iter()
            .filter(|line| bucket_of(&line.category) == Bucket::Credit)
            .map(|line| line.amount_cents)
            .sum();
        by_date.insert(lines[0].date.clone(), credit < 0);
    }
    by_date
}

/// What we know about the day's credit.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Credit {
    Earned,
    Missed,
    Unknown,
}

impl Credit {
    fn as_str(self) -> &'static str {
        match self {
            Self::Earned => "earned",
            Self::Missed => "missed",
            Self::Unknown => "unknown",
        }
    }

    fn from_earned(earned: bool) -> Self {
        if earned { Self::Earned } else { Self::Missed }
    }
}

/// Decides the credit and where the answer came from. GloBird's bill wins
/// when it has the day. Otherwise our estimate counts, but only once the
/// window is over and we have data from it.
fn decide_credit(
    billed: Option<bool>,
    window_over: bool,
    has_window_data: bool,
    estimate_earned: bool,
) -> (Credit, &'static str) {
    if let Some(earned) = billed {
        return (Credit::from_earned(earned), "globird");
    }
    if window_over && has_window_data {
        (Credit::from_earned(estimate_earned), "estimate")
    } else {
        (Credit::Unknown, "estimate")
    }
}

/// The facts that decide why a day missed its credit.
#[derive(Debug, Default)]
struct MissFacts {
    car_charging: bool,
    has_window_data: bool,
    min_soc_in_window: Option<f64>,
    /// The lowest charge seen across all the days: the battery's reserve.
    battery_floor: Option<f64>,
    peak_window_load_kw: Option<f64>,
}

/// Why a day missed the credit. The first rule that fits wins.
fn pick_reason(facts: &MissFacts) -> &'static str {
    if facts.car_charging {
        return "car_charging";
    }
    if !facts.has_window_data {
        return "no_data";
    }
    if let (Some(min), Some(floor)) = (facts.min_soc_in_window, facts.battery_floor)
        && min <= floor + LOW_MARGIN_PCT
    {
        return "battery_low";
    }
    if facts
        .peak_window_load_kw
        .is_some_and(|kw| kw > HIGH_LOAD_KW)
    {
        return "high_load";
    }
    "small_import"
}

/// Whether a car charging session at home overlaps `[from, to)`. A session
/// that has not ended counts as running until `now`.
fn car_charging_during(sessions: &[ChargeSession], from: i64, to: i64, now: i64) -> bool {
    sessions
        .iter()
        .filter(|s| ev_cost::is_home(s))
        .any(|s| s.started_at < to && s.ended_at.unwrap_or(now) > from)
}

// ---------------------------------------------------------------------------
// One day, and the summary
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct DayRow {
    date: String,
    credit: &'static str,
    credit_source: &'static str,
    window_import_kwh: Option<f64>,
    window_export_kwh: Option<f64>,
    window_export_cents: Option<f64>,
    soc_at_11: Option<f64>,
    soc_at_16: Option<f64>,
    soc_at_21: Option<f64>,
    min_soc_in_window: Option<f64>,
    morning_import_kwh: Option<f64>,
    morning_import_cents: Option<f64>,
    reason: Option<&'static str>,
    // The rest is for working out the reason and the summary, not for the client.
    #[serde(skip)]
    car_charging: bool,
    #[serde(skip)]
    peak_window_load_kw: Option<f64>,
    #[serde(skip)]
    credit_cents: f64,
}

impl DayRow {
    /// The lowest charge this day shows anywhere.
    fn lowest_soc(&self) -> Option<f64> {
        [
            self.soc_at_11,
            self.soc_at_16,
            self.soc_at_21,
            self.min_soc_in_window,
        ]
        .into_iter()
        .flatten()
        .min_by(f64::total_cmp)
    }

    fn is_missed(&self) -> bool {
        self.credit == Credit::Missed.as_str()
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The lowest charge seen on any of the days: the battery's reserve.
fn battery_floor(rows: &[DayRow]) -> Option<f64> {
    rows.iter()
        .filter_map(DayRow::lowest_soc)
        .min_by(f64::total_cmp)
}

/// Sets `reason` on every missed day (and only those).
fn add_reasons(rows: &mut [DayRow]) {
    let floor = battery_floor(rows);
    for row in rows.iter_mut().filter(|r| r.is_missed()) {
        row.reason = Some(pick_reason(&MissFacts {
            car_charging: row.car_charging,
            has_window_data: row.window_import_kwh.is_some(),
            min_soc_in_window: row.min_soc_in_window,
            battery_floor: floor,
            peak_window_load_kw: row.peak_window_load_kw,
        }));
    }
}

#[derive(Debug, Serialize, PartialEq)]
struct Summary {
    days: usize,
    earned: usize,
    missed: usize,
    missed_dollars: f64,
    avg_window_export_kwh: Option<f64>,
    avg_window_export_cents: Option<f64>,
    avg_soc_at_11: Option<f64>,
    avg_morning_import_kwh: Option<f64>,
    avg_morning_import_cents: Option<f64>,
    battery_floor_pct: Option<f64>,
}

/// The average of the values that exist, or `None` if none do.
fn average(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let present: Vec<f64> = values.flatten().collect();
    if present.is_empty() {
        None
    } else {
        Some(present.iter().sum::<f64>() / present.len() as f64)
    }
}

fn summarise(rows: &[DayRow]) -> Summary {
    let missed: Vec<&DayRow> = rows.iter().filter(|r| r.is_missed()).collect();
    let missed_cents: f64 = missed.iter().map(|r| r.credit_cents).sum();
    let avg = |get: fn(&DayRow) -> Option<f64>| average(rows.iter().map(get)).map(round2);
    Summary {
        days: rows.len(),
        earned: rows
            .iter()
            .filter(|r| r.credit == Credit::Earned.as_str())
            .count(),
        missed: missed.len(),
        missed_dollars: round2(missed_cents / 100.0),
        avg_window_export_kwh: avg(|r| r.window_export_kwh),
        avg_window_export_cents: avg(|r| r.window_export_cents),
        avg_soc_at_11: avg(|r| r.soc_at_11),
        avg_morning_import_kwh: avg(|r| r.morning_import_kwh),
        avg_morning_import_cents: avg(|r| r.morning_import_cents),
        battery_floor_pct: battery_floor(rows).map(round1),
    }
}

// ---------------------------------------------------------------------------
// Putting a day together
// ---------------------------------------------------------------------------

/// Unix time of `minutes` after local midnight on `date`, or `None` if that
/// moment does not exist (a daylight saving gap).
fn local_time(date: NaiveDate, minutes: u32, tz: Tz) -> Option<i64> {
    let naive = date.and_time(NaiveTime::MIN) + chrono::Duration::minutes(minutes.into());
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(earliest, _) => Some(earliest.timestamp()),
        LocalResult::None => None,
    }
}

/// Everything loaded for one day.
struct DayInput<'a> {
    date: NaiveDate,
    now: i64,
    intervals: Option<&'a [Interval]>,
    soc: &'a SocSeries,
    billed: Option<bool>,
    sessions: &'a [ChargeSession],
}

/// Works out one day's row (without the reason, which needs all the days).
/// `None` if the plan in effect has no credit window.
fn build_day(tariff: &Tariff, tz: Tz, input: &DayInput) -> Option<DayRow> {
    let (window_from, window_to) = tariff.credit_window_minutes()?;
    let free_start = tariff
        .free_import_start()
        .unwrap_or(DEFAULT_FREE_START_MINUTE);
    let date = input.date;
    let at = |minute: u32| local_time(date, minute, tz);
    let (window_start, window_end) = (at(window_from), at(window_to));
    // A period only counts once it is over, so today's averages are not skewed.
    let is_over = |end: Option<i64>| end.is_some_and(|end| input.now >= end);

    let sums = input
        .intervals
        .map(|iv| day_sums(tariff, tz, free_start, iv));
    let window_over = is_over(window_end);
    let sums_window = sums.as_ref().filter(|s| s.has_window_data && window_over);
    let sums_morning = sums.as_ref().filter(|_| is_over(at(free_start)));

    let estimate_earned = input
        .intervals
        .is_some_and(|iv| cost::day_cost(tariff, tz, iv).zero_import_credit_cents > 0.0);
    let (credit, credit_source) = decide_credit(
        input.billed,
        window_over,
        sums.as_ref().is_some_and(|s| s.has_window_data),
        estimate_earned,
    );

    let soc_at = |minute: u32| at(minute).and_then(|ts| input.soc.at(ts)).map(round1);
    let min_soc = window_start
        .zip(window_end)
        .and_then(|(from, to)| input.soc.lowest_between(from, to))
        .map(round1);
    let car_charging = window_start
        .zip(window_end)
        .is_some_and(|(from, to)| car_charging_during(input.sessions, from, to, input.now));

    Some(DayRow {
        date: date.to_string(),
        credit: credit.as_str(),
        credit_source,
        window_import_kwh: sums_window.map(|s| round2(s.window_import_kwh)),
        window_export_kwh: sums_window.map(|s| round2(s.window_export_kwh)),
        window_export_cents: sums_window.map(|s| round2(s.window_export_cents)),
        soc_at_11: soc_at(free_start),
        soc_at_16: soc_at(EVENING_PEAK_MINUTE),
        soc_at_21: soc_at(window_to),
        min_soc_in_window: min_soc,
        morning_import_kwh: sums_morning.map(|s| round2(s.morning_import_kwh)),
        morning_import_cents: sums_morning.map(|s| round2(s.morning_import_cents)),
        reason: None,
        car_charging,
        peak_window_load_kw: sums_window.and_then(|s| s.peak_window_load_kw),
        credit_cents: tariff.credit_cents().unwrap_or(0.0),
    })
}

// ---------------------------------------------------------------------------
// The route
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BatteryParams {
    days: Option<String>,
}

/// The `days` query value: absent means 30, and the number is kept within 1 to 92.
fn parse_days(text: Option<&str>) -> Result<u64, ApiError> {
    match text {
        None => Ok(DEFAULT_DAYS),
        Some(text) => text
            .parse::<u64>()
            .map(|days| days.clamp(1, MAX_DAYS))
            .map_err(|_| ApiError::bad_request("days must be a whole number")),
    }
}

/// `(ts, value)` pairs of one point from `(point_id, ts, value)` rows.
fn point_series(rows: &[(i64, i64, f64)], point_id: i64) -> Vec<(i64, f64)> {
    rows.iter()
        .filter(|row| row.0 == point_id)
        .map(|row| (row.1, row.2))
        .collect()
}

/// `{"days":[...],"summary":{...}}` for the last `days` local days (default
/// 30, at most 92), newest first. `{"configured":false}` without a `[tariff]`
/// that has a `zero_import_credit`.
pub async fn battery(
    State(state): State<AppState>,
    Query(params): Query<BatteryParams>,
) -> ApiResult {
    let count = parse_days(params.days.as_deref())?;
    let Some(schedule) = &state.tariff else {
        return Ok(Json(json!({ "configured": false })));
    };
    let tz = state.tz;
    let today = today(tz);
    if schedule.for_date(today).credit_window_minutes().is_none() {
        return Ok(Json(json!({ "configured": false })));
    }

    // Oldest first, ending today.
    let dates: Vec<NaiveDate> = (0..count)
        .rev()
        .filter_map(|back| today.checked_sub_days(Days::new(back)))
        .collect();
    let (first, last) = (dates[0], dates[dates.len() - 1]);
    let range_start = day_bounds_or_error(first, tz)?.0;
    let range_end = day_bounds_or_error(last, tz)?.1;
    let now = db::now();

    let rows = db::readings_for_points(
        &state.pool,
        state.plant_id,
        &[IMPORT_COUNTER, EXPORT_COUNTER, LOAD_COUNTER, SOC_POINT],
        range_start,
        range_end,
    )
    .await?;
    let import = point_series(&rows, IMPORT_COUNTER);
    let export = point_series(&rows, EXPORT_COUNTER);
    let load = point_series(&rows, LOAD_COUNTER);
    let cloud_soc: Vec<(i64, f64)> = point_series(&rows, SOC_POINT)
        .into_iter()
        .map(|(ts, value)| (ts, display_value(SOC_POINT, value)))
        .collect();
    let sessions = db::charge_sessions_in_range(&state.pool, range_start, range_end, now).await?;
    let billed = billed_credit_by_date(
        &db::globird_cost_in_range(&state.pool, &first.to_string(), &last.to_string()).await?,
    );
    let live_range = db::live_ts_range(&state.pool).await?;

    let mut days = Vec::new();
    for &date in &dates {
        let (start, end) = day_bounds_or_error(date, tz)?;
        // One day of live samples at a time, to keep memory small.
        let live: Vec<Sample> = match live_range {
            Some((oldest, newest)) if oldest < end && newest >= start => {
                db::live_samples_raw(&state.pool, start, end).await?
            }
            _ => Vec::new(),
        };
        let day = DayData {
            start,
            end,
            live: &live,
            import: slice_between(&import, |r| r.0, start, end),
            export: slice_between(&export, |r| r.0, start, end),
            load: slice_between(&load, |r| r.0, start, end),
        };
        let intervals = day_intervals(&day, now).map(|(_, intervals, _)| intervals);
        let soc = SocSeries {
            live: live
                .iter()
                .filter_map(|s| Some((s.ts, s.soc_pct?)))
                .collect(),
            cloud: slice_between(&cloud_soc, |r| r.0, start, end).to_vec(),
        };
        let input = DayInput {
            date,
            now,
            intervals: intervals.as_deref(),
            soc: &soc,
            billed: billed.get(&date.to_string()).copied(),
            sessions: &sessions,
        };
        days.extend(build_day(schedule.for_date(date), tz, &input));
    }

    add_reasons(&mut days);
    let summary = summarise(&days);
    days.reverse();
    Ok(Json(
        json!({ "configured": true, "days": days, "summary": summary }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soc_uses_the_nearest_reading_within_fifteen_minutes() {
        let points = [(1000, 50.0), (2000, 60.0)];
        assert_eq!(nearest_soc(&points, 1100), Some(50.0));
        assert_eq!(nearest_soc(&points, 1600), Some(60.0));
        // 900 s is the limit; one second more is too far.
        assert_eq!(nearest_soc(&[(1000, 50.0)], 1000 + 900), Some(50.0));
        assert_eq!(nearest_soc(&[(1000, 50.0)], 1000 + 901), None);
        assert_eq!(nearest_soc(&[], 1000), None);
    }

    #[test]
    fn lowest_soc_looks_only_inside_the_range() {
        let points = [(10, 5.0), (20, 40.0), (30, 38.0), (40, 2.0)];
        assert_eq!(lowest_soc(&points, 20, 30), Some(38.0));
        assert_eq!(lowest_soc(&points, 50, 60), None);
    }

    #[test]
    fn live_charge_wins_and_cloud_fills_the_gaps() {
        let soc = SocSeries {
            live: vec![(1000, 80.0)],
            cloud: vec![(1000, 70.0), (5000, 30.0)],
        };
        assert_eq!(soc.at(1000), Some(80.0));
        assert_eq!(soc.at(5000), Some(30.0));
        assert_eq!(soc.at(9000), None);
        assert_eq!(soc.lowest_between(0, 2000), Some(80.0));
        assert_eq!(soc.lowest_between(4000, 6000), Some(30.0));
    }

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    fn zerohero() -> Tariff {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 44.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "16:00", to = "23:00", cents = 60.0 },
            ]
            default_export_cents = 0.0
            export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
            [tariff.zero_import_credit]
            from = "18:00"
            to = "21:00"
            max_kwh = 0.1
            credit_cents = 100.0
            "#,
        )
        .unwrap();
        Tariff::from_config(&w.tariff).unwrap()
    }

    /// Unix time of a local Adelaide time on 2026-09-15 (+09:30).
    fn at(hour: u32, minute: u32) -> i64 {
        local_time(
            NaiveDate::from_ymd_opt(2026, 9, 15).unwrap(),
            hour * 60 + minute,
            ADELAIDE,
        )
        .unwrap()
    }

    fn iv(from: (u32, u32), to: (u32, u32), import: f64, export: f64, load: f64) -> Interval {
        Interval {
            start_ts: at(from.0, from.1),
            end_ts: at(to.0, to.1),
            import_kwh: import,
            export_kwh: export,
            load_kwh: load,
        }
    }

    #[test]
    fn sums_split_the_morning_and_the_window() {
        let tariff = zerohero();
        let intervals = [
            iv((7, 0), (7, 5), 0.5, 0.0, 0.5),    // morning, 44 c
            iv((10, 55), (11, 0), 0.1, 0.0, 0.1), // morning, 44 c
            iv((12, 0), (12, 5), 2.0, 0.0, 2.0),  // free power, not morning
            iv((18, 0), (18, 5), 0.0, 1.0, 0.2),  // window
            iv((19, 0), (19, 5), 0.05, 0.5, 0.6), // window
            iv((22, 0), (22, 5), 1.0, 0.0, 1.0),  // after the window
        ];
        let sums = day_sums(&tariff, ADELAIDE, 11 * 60, &intervals);
        assert!(sums.has_window_data);
        assert!((sums.window_import_kwh - 0.05).abs() < 1e-9);
        assert!((sums.window_export_kwh - 1.5).abs() < 1e-9);
        assert!((sums.window_export_cents - 15.0).abs() < 1e-9);
        assert!((sums.morning_import_kwh - 0.6).abs() < 1e-9);
        assert!((sums.morning_import_cents - 26.4).abs() < 1e-9);
        // 0.6 kWh in 5 minutes is 7.2 kW.
        assert!((sums.peak_window_load_kw.unwrap() - 7.2).abs() < 1e-9);
    }

    #[test]
    fn a_day_with_nothing_in_the_window_has_no_window_data() {
        let tariff = zerohero();
        let sums = day_sums(&tariff, ADELAIDE, 660, &[iv((9, 0), (9, 5), 0.2, 0.0, 0.2)]);
        assert!(!sums.has_window_data);
        assert_eq!(sums.peak_window_load_kw, None);
    }

    #[test]
    fn short_spells_do_not_count_as_peak_load() {
        // Many 15-second samples add up to one 5-minute spell: 0.1 kWh in 300 s is 1.2 kW.
        let mut spells = BTreeMap::new();
        spells.insert(1, (0.1, 300));
        // A single 10-second blip with a big number is ignored.
        spells.insert(2, (0.5, 10));
        assert!((peak_load_kw(&spells).unwrap() - 1.2).abs() < 1e-9);
        assert_eq!(peak_load_kw(&BTreeMap::new()), None);
    }

    fn cost_row(date: &str, category: &str, cents: i64) -> GlobirdCostRow {
        GlobirdCostRow {
            date: date.to_string(),
            category: category.to_string(),
            amount_cents: cents,
            quantity: 0.0,
        }
    }

    #[test]
    fn globird_rows_say_whether_the_credit_was_earned() {
        let rows = [
            cost_row("2026-10-08", "SUPPLY", 174),
            cost_row("2026-10-09", "SUPPLY", 174),
            cost_row("2026-10-09", "USAGE", 75),
            cost_row("2026-10-09", "ZEROHERO Credit", -100),
            cost_row("2026-10-10", "SUPPLY", 174),
            cost_row("2026-10-10", "USAGE", 75),
        ];
        let by_date = billed_credit_by_date(&rows);
        // The 8th has only its SUPPLY line, so it is not complete.
        assert_eq!(by_date.get("2026-10-08"), None);
        assert_eq!(by_date.get("2026-10-09"), Some(&true));
        assert_eq!(by_date.get("2026-10-10"), Some(&false));
    }

    #[test]
    fn globird_beats_our_estimate_and_today_stays_unknown() {
        assert_eq!(
            decide_credit(Some(true), true, true, false),
            (Credit::Earned, "globird")
        );
        assert_eq!(
            decide_credit(Some(false), true, true, true),
            (Credit::Missed, "globird")
        );
        assert_eq!(
            decide_credit(None, true, true, true),
            (Credit::Earned, "estimate")
        );
        assert_eq!(
            decide_credit(None, true, true, false),
            (Credit::Missed, "estimate")
        );
        // Window not over yet, or no data in it: we cannot say.
        assert_eq!(
            decide_credit(None, false, true, true),
            (Credit::Unknown, "estimate")
        );
        assert_eq!(
            decide_credit(None, true, false, true),
            (Credit::Unknown, "estimate")
        );
    }

    fn facts() -> MissFacts {
        MissFacts {
            car_charging: false,
            has_window_data: true,
            min_soc_in_window: Some(40.0),
            battery_floor: Some(10.0),
            peak_window_load_kw: Some(2.0),
        }
    }

    #[test]
    fn reasons_are_picked_in_order() {
        assert_eq!(pick_reason(&facts()), "small_import");

        let high = MissFacts {
            peak_window_load_kw: Some(6.5),
            ..facts()
        };
        assert_eq!(pick_reason(&high), "high_load");

        // Within 3 points of the floor counts as low, 3.1 does not.
        let low = MissFacts {
            min_soc_in_window: Some(13.0),
            ..high
        };
        assert_eq!(pick_reason(&low), "battery_low");
        let not_low = MissFacts {
            min_soc_in_window: Some(13.1),
            ..low
        };
        assert_eq!(pick_reason(&not_low), "high_load");

        let car = MissFacts {
            car_charging: true,
            min_soc_in_window: Some(10.0),
            ..not_low
        };
        assert_eq!(pick_reason(&car), "car_charging");

        let nothing = MissFacts {
            has_window_data: false,
            ..facts()
        };
        assert_eq!(pick_reason(&nothing), "no_data");
        // Without a floor to compare with, low battery cannot be called.
        let no_floor = MissFacts {
            battery_floor: None,
            ..facts()
        };
        assert_eq!(pick_reason(&no_floor), "small_import");
    }

    fn session(started_at: i64, ended_at: Option<i64>, geofence: Option<&str>) -> ChargeSession {
        ChargeSession {
            id: 1,
            started_at,
            ended_at,
            energy_added_kwh: None,
            energy_used_kwh: None,
            start_soc_pct: None,
            end_soc_pct: None,
            max_power_kw: None,
            geofence: geofence.map(str::to_string),
        }
    }

    #[test]
    fn car_sessions_must_overlap_the_window_and_be_at_home() {
        let (from, to) = (1000, 2000);
        let home = Some("Home");
        assert!(car_charging_during(
            &[session(500, Some(1500), home)],
            from,
            to,
            9999
        ));
        assert!(car_charging_during(
            &[session(1500, Some(3000), None)],
            from,
            to,
            9999
        ));
        // Still charging counts as running until now.
        assert!(car_charging_during(
            &[session(500, None, home)],
            from,
            to,
            9999
        ));
        assert!(!car_charging_during(
            &[session(500, None, home)],
            from,
            to,
            900
        ));
        // Ends exactly when the window starts, or starts when it ends.
        assert!(!car_charging_during(
            &[session(500, Some(1000), home)],
            from,
            to,
            9999
        ));
        assert!(!car_charging_during(
            &[session(2000, Some(3000), home)],
            from,
            to,
            9999
        ));
        // Somewhere else does not use our battery.
        assert!(!car_charging_during(
            &[session(500, Some(1500), Some("Work"))],
            from,
            to,
            9999
        ));
    }

    fn row(
        credit: Credit,
        export: Option<f64>,
        soc11: Option<f64>,
        min_soc: Option<f64>,
    ) -> DayRow {
        DayRow {
            date: "2026-09-15".to_string(),
            credit: credit.as_str(),
            credit_source: "estimate",
            window_import_kwh: export.map(|_| 0.0),
            window_export_kwh: export,
            window_export_cents: export.map(|kwh| kwh * 10.0),
            soc_at_11: soc11,
            soc_at_16: None,
            soc_at_21: None,
            min_soc_in_window: min_soc,
            morning_import_kwh: Some(1.0),
            morning_import_cents: Some(44.0),
            reason: None,
            car_charging: false,
            peak_window_load_kw: Some(1.0),
            credit_cents: 100.0,
        }
    }

    #[test]
    fn reasons_use_the_floor_learnt_from_all_days() {
        let mut rows = vec![
            row(Credit::Earned, Some(10.0), Some(90.0), Some(12.0)),
            row(Credit::Missed, Some(8.0), Some(90.0), Some(14.5)),
            row(Credit::Missed, Some(8.0), Some(90.0), Some(50.0)),
            row(Credit::Unknown, None, None, None),
        ];
        assert_eq!(battery_floor(&rows), Some(12.0));
        add_reasons(&mut rows);
        assert_eq!(rows[0].reason, None);
        assert_eq!(rows[1].reason, Some("battery_low"));
        assert_eq!(rows[2].reason, Some("small_import"));
        assert_eq!(rows[3].reason, None);
    }

    #[test]
    fn summary_counts_days_and_averages_what_exists() {
        let rows = vec![
            row(Credit::Earned, Some(10.0), Some(90.0), Some(30.0)),
            row(Credit::Missed, Some(5.0), Some(70.0), Some(10.0)),
            row(Credit::Missed, Some(6.0), None, None),
            row(Credit::Unknown, None, None, None),
        ];
        let summary = summarise(&rows);
        assert_eq!(summary.days, 4);
        assert_eq!(summary.earned, 1);
        assert_eq!(summary.missed, 2);
        assert_eq!(summary.missed_dollars, 2.0);
        assert_eq!(summary.avg_window_export_kwh, Some(7.0));
        assert_eq!(summary.avg_window_export_cents, Some(70.0));
        assert_eq!(summary.avg_soc_at_11, Some(80.0));
        assert_eq!(summary.battery_floor_pct, Some(10.0));
        let empty = summarise(&[]);
        assert_eq!(empty.days, 0);
        assert_eq!(empty.avg_soc_at_11, None);
        assert_eq!(empty.battery_floor_pct, None);
    }

    #[test]
    fn days_parameter_defaults_and_clamps() {
        assert_eq!(parse_days(None).unwrap(), 30);
        assert_eq!(parse_days(Some("7")).unwrap(), 7);
        assert_eq!(parse_days(Some("0")).unwrap(), 1);
        assert_eq!(parse_days(Some("500")).unwrap(), 92);
        assert!(parse_days(Some("x")).is_err());
        assert!(parse_days(Some("-3")).is_err());
    }

    #[test]
    fn local_time_follows_daylight_saving() {
        // Adelaide's clocks went forward on 2026-10-04 (02:00 -> 03:00): 11:00 is +10:30 then.
        let date = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let utc_11 = chrono::Utc
            .with_ymd_and_hms(2026, 10, 5, 0, 30, 0)
            .unwrap()
            .timestamp();
        assert_eq!(local_time(date, 11 * 60, ADELAIDE), Some(utc_11));
    }
}
