//! `/api/costs`: what each local day cost, and what it would have cost
//! without solar, using the `[tariff]` from the config.
//!
//! This file turns stored data into the [`Interval`]s that `cost::day_cost`
//! prices. The interval builders are pure, so they are tested with plain numbers.

use std::collections::{BTreeMap, HashMap};
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
use crate::cost::{self, DayCost, GridSplit, Interval, Tariff, TariffSchedule};
use crate::db::{self, GlobirdCostRow, GlobirdDayKwh};
use crate::globird::data::{self as globird_data, bucket_of, is_supply};
use crate::modbus::Sample;

/// The whole history since Feb 2023 is about 1,000 days; this leaves room to grow.
const MAX_COSTS_DAYS: u64 = 1500;
/// The longest range for `group=month` or `group=year`: 20 years. The answer is
/// a few hundred small rows at most, and days already stored are not worked out again.
pub(super) const MAX_GROUPED_COSTS_DAYS: u64 = 366 * 20;
/// Live samples are loaded this many days at a time. A year of them is
/// millions of rows, too many to hold at once on a small machine.
const LIVE_CHUNK_DAYS: usize = 31;

/// The cloud counters (Wh, reset at local midnight) the cloud cost comes from.
pub(super) const IMPORT_COUNTER: i64 = 83102;
pub(super) const EXPORT_COUNTER: i64 = 83072;
pub(super) const LOAD_COUNTER: i64 = 83118;

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
pub(super) struct DayData<'a> {
    pub start: i64,
    pub end: i64,
    pub live: &'a [Sample],
    pub import: &'a [(i64, f64)],
    pub export: &'a [(i64, f64)],
    pub load: &'a [(i64, f64)],
}

/// The intervals of one day and where they came from, or `None` if there is
/// no data. Live samples win when they cover the day well, the same test as
/// `/api/daily` uses; a day they cover only partly falls back to the cloud
/// counters, unless the cloud has nothing for the day. The last flag is
/// whether the live samples covered the day well.
pub(super) fn day_intervals(
    day: &DayData,
    now: i64,
) -> Option<(&'static str, Vec<Interval>, bool)> {
    let cloud_has_data = !(day.import.is_empty() && day.export.is_empty() && day.load.is_empty());
    if let Some(live) = live_totals(day.live, day.start, day.end, now)
        && live.usable(cloud_has_data)
    {
        return Some(("inverter", live_intervals(day.live), live.solid));
    }
    if !cloud_has_data {
        return None;
    }
    let intervals = cloud_intervals(day.start, day.import, day.export, day.load);
    Some(("cloud", intervals, true))
}

/// One day's grid energy split by tariff band, priced on the plan in effect
/// that day, or `None` when the day has no interval data (or no tariff).
///
/// This uses [`day_intervals`], the same intervals `/api/costs` prices, so the
/// parts add up to its `import_kwh` and `export_kwh`. `live` is the day's live
/// samples as `/api/daily` already loaded them; the cloud counters are read
/// here, and only when the live samples do not cover the day well (a solid
/// live day never looks at them). The split can differ slightly from
/// `/api/daily`'s own `import_kwh`: that reads the cloud's daily counter total
/// (or the live integral), while the split sums the intervals built from the
/// 5-minute counter steps, which can lose the first reading or cap long gaps.
pub(super) async fn day_grid_split(
    state: &AppState,
    schedule: &TariffSchedule,
    date: NaiveDate,
    live: &[Sample],
    now: i64,
) -> Result<Option<GridSplit>, ApiError> {
    let (start, end) = day_bounds_or_error(date, state.tz)?;
    let solid = live_totals(live, start, end, now).is_some_and(|l| l.solid);
    let rows = if solid {
        Vec::new()
    } else {
        db::readings_for_points(
            &state.pool,
            state.plant_id,
            &[IMPORT_COUNTER, EXPORT_COUNTER, LOAD_COUNTER],
            start,
            end,
        )
        .await?
    };
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
    let day = DayData {
        start,
        end,
        live,
        import: &import,
        export: &export,
        load: &load,
    };
    Ok(day_intervals(&day, now)
        .map(|(_, intervals, _)| cost::grid_split(schedule.for_date(date), state.tz, &intervals)))
}

/// The rows of a list sorted by time that fall in `[start, end)`.
pub(super) fn slice_between<T>(
    rows: &[T],
    ts_of: impl Fn(&T) -> i64,
    start: i64,
    end: i64,
) -> &[T] {
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
    /// "inverter" (live samples) or "cloud", as in `/api/daily`, or "globird"
    /// when the money is from GloBird's own bill for the day.
    source: &'static str,
    actual: f64,
    baseline: f64,
    savings: f64,
    import_kwh: f64,
    export_kwh: f64,
    load_kwh: f64,
    /// Dollars of zero-import credit earned this day.
    credit: f64,
    /// Dollars of daily supply charge.
    supply: f64,
    /// Dollars paid for grid import.
    import: f64,
    /// Dollars earned for feed-in (export).
    feed_in: f64,
}

/// How `/api/costs` groups its rows: one per day, or summed per month or year.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Group {
    Day,
    Month,
    Year,
}

impl Group {
    /// The `group` query value; absent means days.
    pub(super) fn parse(text: Option<&str>) -> Result<Self, ApiError> {
        match text {
            None | Some("day") => Ok(Self::Day),
            Some("month") => Ok(Self::Month),
            Some("year") => Ok(Self::Year),
            Some(_) => Err(ApiError::bad_request("group must be day, month or year")),
        }
    }

    /// The bucket a day falls in: "2026-09" for a month, "2026" for a year.
    pub(super) fn period_of(self, date: NaiveDate) -> String {
        match self {
            Self::Day => date.to_string(),
            Self::Month => date.format("%Y-%m").to_string(),
            Self::Year => date.format("%Y").to_string(),
        }
    }
}

/// Costs added up over any number of days, kept in unrounded cents so the
/// sums do not drift from the true total by a cent per day.
#[derive(Debug, Default, Clone, PartialEq)]
struct Sums {
    /// Days with data.
    days: u32,
    /// Days that earned the zero-import credit.
    credit_days: u32,
    /// Days whose money is GloBird's own bill.
    billed_days: u32,
    supply_cents: f64,
    import_cents: f64,
    export_cents: f64,
    credit_cents: f64,
    actual_cents: f64,
    baseline_cents: f64,
    import_kwh: f64,
    export_kwh: f64,
    load_kwh: f64,
}

impl Sums {
    /// Like [`Sums::add`], and counts the day if it is GloBird's bill.
    fn add_day(&mut self, day: &CostedDay) {
        self.add(&day.cost);
        if day.source == SOURCE_GLOBIRD {
            self.billed_days += 1;
        }
    }

    fn add(&mut self, cost: &DayCost) {
        self.days += 1;
        if cost.zero_import_credit_cents > 0.0 {
            self.credit_days += 1;
        }
        self.supply_cents += cost.supply_cents;
        self.import_cents += cost.import_cents;
        self.export_cents += cost.export_cents;
        self.credit_cents += cost.zero_import_credit_cents;
        self.actual_cents += cost.actual_cents;
        self.baseline_cents += cost.baseline_cents;
        self.import_kwh += cost.import_kwh;
        self.export_kwh += cost.export_kwh;
        self.load_kwh += cost.load_kwh;
    }

    /// The value of power not bought: what the load would have cost on the
    /// grid (baseline), less what was still paid for supply and import.
    /// `savings == not_bought + feed-in + credit`.
    fn not_bought_cents(&self) -> f64 {
        self.baseline_cents - self.supply_cents - self.import_cents
    }

    /// The bill-style breakdown, in dollars and kWh.
    fn totals(&self) -> Totals {
        Totals {
            days: self.days,
            credit_days: self.credit_days,
            billed_days: self.billed_days,
            supply: dollars(self.supply_cents),
            import: dollars(self.import_cents),
            import_kwh: round2(self.import_kwh),
            feed_in: dollars(self.export_cents),
            export_kwh: round2(self.export_kwh),
            load_kwh: round2(self.load_kwh),
            credit: dollars(self.credit_cents),
            actual: dollars(self.actual_cents),
            baseline: dollars(self.baseline_cents),
            savings: dollars(self.baseline_cents - self.actual_cents),
            not_bought: dollars(self.not_bought_cents()),
        }
    }
}

/// A bill-style breakdown of a period, in dollars and kWh.
#[derive(Debug, Serialize)]
struct Totals {
    /// Days with data.
    days: u32,
    /// Days that earned the zero-import credit.
    credit_days: u32,
    /// Days whose money is GloBird's own bill rather than our estimate.
    billed_days: u32,
    supply: f64,
    import: f64,
    import_kwh: f64,
    /// Feed-in earnings, as a positive number.
    feed_in: f64,
    export_kwh: f64,
    load_kwh: f64,
    /// Zero-import credit earned.
    credit: f64,
    /// What was really paid.
    actual: f64,
    /// What it would have cost with no solar or battery.
    baseline: f64,
    savings: f64,
    /// Value of the power not bought from the grid.
    not_bought: f64,
}

/// A month or year of days added up.
#[derive(Debug, Serialize)]
struct Bucket {
    /// "2026-09" for a month, "2026" for a year.
    period: String,
    #[serde(flatten)]
    totals: Totals,
}

/// Adds up days (oldest first) into one [`Bucket`] per period that has data.
fn bucket_days(group: Group, costed: &[CostedDay]) -> Vec<Bucket> {
    let mut buckets: Vec<(String, Sums)> = Vec::new();
    for day in costed {
        let period = group.period_of(day.date);
        if buckets.last().is_none_or(|(last, _)| *last != period) {
            buckets.push((period, Sums::default()));
        }
        if let Some((_, sums)) = buckets.last_mut() {
            sums.add_day(day);
        }
    }
    buckets
        .into_iter()
        .map(|(period, sums)| Bucket {
            period,
            totals: sums.totals(),
        })
        .collect()
}

/// One priced local day, whether it came from the cache or was just worked out.
#[derive(Debug, Clone)]
pub(super) struct CostedDay {
    pub date: NaiveDate,
    /// "inverter" or "cloud", or "globird" once [`billed_days`] has put
    /// GloBird's bill in. Stored rows are never "globird".
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

/// Bump this when the way a day is costed changes, so days stored by the old
/// rules are worked out again. Version 2: finished days need 98% live coverage
/// (not 90%) to use the inverter, and the zero-import credit waits for the
/// window to finish. Old rows are replaced by date as days are recomputed.
const COST_CACHE_VERSION: u32 = 2;

/// What stored costs are tagged with. The time zone is in it too, because
/// moving it moves where every local day starts.
fn cache_key(tz: Tz, tariff: &Tariff) -> String {
    format!("{COST_CACHE_VERSION}|{tz}|{}", tariff.key())
}

/// May this day's cost be stored for good? Only a finished day with all its
/// data: it is before `today`, and either it used the inverter with good
/// coverage (`live_solid`; a partial live day is kept out so a cloud backfill
/// can replace it later) or the cloud counters were read in the day's last 10
/// minutes (23:50 local or later). `newest_counter_ts` is the latest cloud
/// reading in the day. A day that fails this is worked out again next time, so
/// a backfill that fills it in later is picked up.
fn is_cacheable(
    date: NaiveDate,
    today: NaiveDate,
    source: &str,
    live_solid: bool,
    newest_counter_ts: Option<i64>,
    day_end: i64,
) -> bool {
    if date >= today {
        return false;
    }
    // Ten minutes before the end is 23:50 local, even on a daylight saving change.
    if source == "inverter" {
        return live_solid;
    }
    newest_counter_ts.is_some_and(|ts| ts >= day_end - 10 * 60)
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
            let Some((source, intervals, live_solid)) = day_intervals(&day, now) else {
                continue;
            };
            let newest_counter_ts = [day.import, day.export, day.load]
                .iter()
                .filter_map(|c| c.last().map(|r| r.0))
                .max();
            let cacheable = is_cacheable(date, today, source, live_solid, newest_counter_ts, end);
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

/// The kWh GloBird metered on one day. `None` means it has no figure.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct DayKwh {
    pub import: Option<f64>,
    pub export: Option<f64>,
}

/// Groups the kWh rows by day. The query already chose, per direction, the
/// interval total or else the charge-period total.
pub(super) fn kwh_by_date(rows: Vec<GlobirdDayKwh>) -> BTreeMap<String, DayKwh> {
    let mut by_date: BTreeMap<String, DayKwh> = BTreeMap::new();
    for row in rows {
        let day = by_date.entry(row.date).or_default();
        match row.direction.as_str() {
            "import" => day.import = Some(row.kwh),
            "export" => day.export = Some(row.kwh),
            _ => {}
        }
    }
    by_date
}

/// The `source` of a day whose money is GloBird's own bill.
pub(super) const SOURCE_GLOBIRD: &str = "globird";

/// GloBird's bill for one complete day, in whole cents (a credit is negative,
/// as GloBird signs it), added up per bucket.
#[derive(Debug, Clone, Default, PartialEq)]
struct Bill {
    import: i64,
    export: i64,
    supply: i64,
    credit: i64,
    other: i64,
    kwh: DayKwh,
}

impl Bill {
    /// The bill from a day's cost lines, or `None` while the day is incomplete
    /// (GloBird has only posted the SUPPLY line so far).
    fn from_lines(lines: &[GlobirdCostRow], kwh: DayKwh) -> Option<Self> {
        if lines.iter().all(|line| is_supply(&line.category)) {
            return None;
        }
        let mut bill = Self {
            kwh,
            ..Self::default()
        };
        for line in lines {
            match bucket_of(&line.category) {
                globird_data::Bucket::Import => bill.import += line.amount_cents,
                globird_data::Bucket::Export => bill.export += line.amount_cents,
                globird_data::Bucket::Supply => bill.supply += line.amount_cents,
                globird_data::Bucket::Credit => bill.credit += line.amount_cents,
                globird_data::Bucket::Other => bill.other += line.amount_cents,
            }
        }
        Some(bill)
    }

    /// What the day cost: every line added up.
    fn total(&self) -> i64 {
        self.import + self.export + self.supply + self.credit + self.other
    }
}

/// Our estimate of a day with GloBird's real figures put in. Money comes from
/// the bill (feed-in and credit turned positive, as `DayCost` holds them) and
/// the grid kWh from the meter, when GloBird has them. The load and the
/// no-solar baseline stay ours, since GloBird can know neither. Savings are
/// worked out again from the new bill.
fn apply_bill(cost: &DayCost, bill: &Bill) -> DayCost {
    let actual_cents = bill.total() as f64;
    DayCost {
        import_kwh: bill.kwh.import.unwrap_or(cost.import_kwh),
        export_kwh: bill.kwh.export.unwrap_or(cost.export_kwh),
        import_cents: bill.import as f64,
        export_cents: -bill.export as f64,
        supply_cents: bill.supply as f64,
        zero_import_credit_cents: -bill.credit as f64,
        actual_cents,
        savings_cents: cost.baseline_cents - actual_cents,
        ..cost.clone()
    }
}

/// Like [`costed_days`], but a day GloBird has billed completely shows
/// GloBird's figures (source "globird") instead of our estimate. A day without
/// our estimate stays out, as we would have no baseline for it. The stored
/// `daily_cost` rows are untouched; this is applied each time it is read, with
/// one range query on the GloBird tables.
///
/// Use this to report money. Anything that compares with GloBird (the Bill
/// check) must use [`costed_days`], or it would compare GloBird with itself.
pub(super) async fn billed_days(
    state: &AppState,
    schedule: &TariffSchedule,
    dates: &[NaiveDate],
) -> Result<Vec<CostedDay>, ApiError> {
    let mut costed = costed_days(state, schedule, dates).await?;
    let (Some(first), Some(last)) = (dates.first(), dates.last()) else {
        return Ok(costed);
    };
    let (from, to) = (first.to_string(), last.to_string());
    let cost_rows = db::globird_cost_in_range(&state.pool, &from, &to).await?;
    if cost_rows.is_empty() {
        return Ok(costed);
    }
    let mut kwh = kwh_by_date(db::globird_kwh_in_range(&state.pool, &from, &to).await?);

    // The rows come ordered by date, so a day's lines are next to each other.
    for lines in cost_rows.chunk_by(|a, b| a.date == b.date) {
        let day_kwh = kwh.remove(&lines[0].date).unwrap_or_default();
        let Some(bill) = Bill::from_lines(lines, day_kwh) else {
            continue;
        };
        // `costed` is in date order, so the day is found by binary search.
        let Ok(date) = NaiveDate::from_str(&lines[0].date) else {
            continue;
        };
        if let Ok(i) = costed.binary_search_by_key(&date, |day| day.date) {
            costed[i].cost = apply_bill(&costed[i].cost, &bill);
            costed[i].source = SOURCE_GLOBIRD;
        }
    }
    Ok(costed)
}

#[derive(Deserialize)]
pub struct CostsParams {
    from: Option<String>,
    to: Option<String>,
    /// "day" (the default), "month" or "year".
    group: Option<String>,
}

pub async fn costs(State(state): State<AppState>, Query(params): Query<CostsParams>) -> ApiResult {
    let group = Group::parse(params.group.as_deref())?;
    let max_days = match group {
        Group::Day => MAX_COSTS_DAYS,
        Group::Month | Group::Year => MAX_GROUPED_COSTS_DAYS,
    };
    let (from, count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        max_days,
        DEFAULT_DAILY_DAYS,
    )?;
    let Some(tariff) = &state.tariff else {
        return Ok(Json(json!({ "configured": false })));
    };

    let dates: Vec<NaiveDate> = from.iter_days().take(count as usize).collect();
    let costed = billed_days(&state, tariff, &dates).await?;

    let mut sums = Sums::default();
    for day in &costed {
        sums.add_day(day);
    }
    let totals = sums.totals();

    if group != Group::Day {
        return Ok(Json(json!({
            "configured": true,
            "group": if group == Group::Month { "month" } else { "year" },
            "totals": totals,
            "buckets": bucket_days(group, &costed),
        })));
    }

    let days: Vec<CostDay> = costed
        .iter()
        .map(|CostedDay { date, source, cost }| CostDay {
            date: date.to_string(),
            source,
            actual: dollars(cost.actual_cents),
            baseline: dollars(cost.baseline_cents),
            savings: dollars(cost.savings_cents),
            import_kwh: round2(cost.import_kwh),
            export_kwh: round2(cost.export_kwh),
            load_kwh: round2(cost.load_kwh),
            credit: dollars(cost.zero_import_credit_cents),
            supply: dollars(cost.supply_cents),
            import: dollars(cost.import_cents),
            feed_in: dollars(cost.export_cents),
        })
        .collect();
    Ok(Json(json!({
        "configured": true,
        "group": "day",
        "totals": totals,
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
        let (source, intervals, _) = day_intervals(&some, 100_000).unwrap();
        assert_eq!(source, "cloud");
        assert_eq!(intervals.len(), 1);
    }

    /// A finished day of live samples every 60 s, from midnight to `last_ts`.
    fn live_until(last_ts: i64) -> Vec<Sample> {
        (0..=last_ts / 60)
            .map(|i| Sample {
                ts: i * 60,
                pv_w: Some(1000.0),
                load_w: Some(500.0),
                grid_w: Some(0.0),
                battery_w: Some(0.0),
                soc_pct: None,
            })
            .collect()
    }

    #[test]
    fn a_partly_covered_day_prefers_the_cloud_but_never_goes_blank() {
        let cloud = [(300, 10.0), (600, 20.0)];
        let day = |live: &'_ [Sample], cloud: &'_ [(i64, f64)]| {
            let day = DayData {
                start: 0,
                end: 86_400,
                live,
                import: cloud,
                export: &[],
                load: &[],
            };
            day_intervals(&day, 1_000_000).map(|(source, _, solid)| (source, solid))
        };
        let (at_95, at_99) = (live_until(82_080), live_until(85_500));
        // 95% live with cloud present: the cloud wins.
        assert_eq!(day(&at_95, &cloud), Some(("cloud", true)));
        // 95% live and no cloud: the live numbers stand, but are not cacheable.
        assert_eq!(day(&at_95, &[]), Some(("inverter", false)));
        // 99% live wins over the cloud.
        assert_eq!(day(&at_99, &cloud), Some(("inverter", true)));
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
            true,
            Some(end - 600),
            end
        ));
        assert!(!is_cacheable(
            yesterday,
            today,
            "cloud",
            true,
            Some(end - 601),
            end
        ));
        assert!(!is_cacheable(yesterday, today, "cloud", true, None, end));
        // Inverter days already passed the coverage test.
        assert!(is_cacheable(yesterday, today, "inverter", true, None, end));
        // A partly covered live day is not kept, so a cloud backfill can replace it.
        assert!(!is_cacheable(
            yesterday, today, "inverter", false, None, end
        ));
        // Today is never cached, nor is the future.
        assert!(!is_cacheable(
            today,
            today,
            "inverter",
            true,
            Some(end),
            end
        ));
        assert!(!is_cacheable(
            date("2026-09-17"),
            today,
            "cloud",
            true,
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

    /// A priced day: supply 100 c, import 40 c, feed-in 25 c, load priced at 90 c.
    fn day_cost_of(date_text: &str, credit_cents: f64) -> CostedDay {
        let actual = 100.0 + 40.0 - 25.0 - credit_cents;
        CostedDay {
            date: date(date_text),
            source: "cloud",
            cost: DayCost {
                import_kwh: 2.0,
                export_kwh: 3.0,
                load_kwh: 4.0,
                import_cents: 40.0,
                export_cents: 25.0,
                supply_cents: 100.0,
                zero_import_credit_cents: credit_cents,
                actual_cents: actual,
                baseline_cents: 190.0,
                savings_cents: 190.0 - actual,
            },
        }
    }

    #[test]
    fn savings_are_power_not_bought_plus_feed_in_plus_credit() {
        let mut sums = Sums::default();
        sums.add(&day_cost_of("2026-09-01", 100.0).cost);
        sums.add(&day_cost_of("2026-09-02", 0.0).cost);
        // Baseline 380 - supply 200 - import 80 = 100 c not bought.
        assert_eq!(sums.not_bought_cents(), 100.0);
        let savings = sums.baseline_cents - sums.actual_cents;
        assert_eq!(
            savings,
            sums.not_bought_cents() + sums.export_cents + sums.credit_cents
        );
        let totals = sums.totals();
        assert_eq!((totals.days, totals.credit_days), (2, 1));
        assert_eq!(totals.not_bought, 1.0);
        assert_eq!(totals.feed_in, 0.5);
        assert_eq!(totals.credit, 1.0);
        assert_eq!(totals.savings, 2.5);
    }

    fn globird_line(category: &str, amount_cents: i64) -> GlobirdCostRow {
        GlobirdCostRow {
            date: "2026-09-01".to_string(),
            category: category.to_string(),
            amount_cents,
            quantity: 1.0,
        }
    }

    /// GloBird's 9 October bill: 75 usage, 32 + 118 + 35 export, 100 credit,
    /// 174 supply and a made-up 10 c line we have no bucket for.
    fn oct_9_bill(kwh: DayKwh) -> Bill {
        let lines = [
            globird_line("USAGE", 75),
            globird_line("SOLAR", -32),
            globird_line("Super Export top up", -118),
            globird_line("Critical Peak - Export", -35),
            globird_line("ZEROHERO Credit", -100),
            globird_line("SUPPLY", 174),
            globird_line("Something new", 10),
        ];
        Bill::from_lines(&lines, kwh).unwrap()
    }

    #[test]
    fn a_bill_replaces_the_money_and_grid_kwh_of_an_estimate() {
        let ours = day_cost_of("2026-09-01", 0.0).cost;
        let kwh = DayKwh {
            import: Some(20.5),
            export: None,
        };
        let billed = apply_bill(&ours, &oct_9_bill(kwh));
        // Positive, as `DayCost` holds them.
        assert_eq!(billed.import_cents, 75.0);
        assert_eq!(billed.export_cents, 185.0);
        assert_eq!(billed.supply_cents, 174.0);
        assert_eq!(billed.zero_import_credit_cents, 100.0);
        // The bill is every line: 75 - 185 - 100 + 174 + 10.
        assert_eq!(billed.actual_cents, -26.0);
        // Savings: our baseline (190) less the bill.
        assert_eq!(billed.baseline_cents, 190.0);
        assert_eq!(billed.savings_cents, 216.0);
        // The meter's import replaces ours; with no export figure ours stays.
        assert_eq!(billed.import_kwh, 20.5);
        assert_eq!(billed.export_kwh, ours.export_kwh);
        // GloBird cannot know the load.
        assert_eq!(billed.load_kwh, ours.load_kwh);
    }

    #[test]
    fn a_day_with_only_the_supply_charge_has_no_bill_yet() {
        let lines = [globird_line("SUPPLY", 174), globird_line(" supply ", 1)];
        assert_eq!(Bill::from_lines(&lines, DayKwh::default()), None);
        assert_eq!(Bill::from_lines(&[], DayKwh::default()), None);
    }

    #[test]
    fn billed_days_are_counted_in_the_totals() {
        let mut billed = day_cost_of("2026-09-01", 0.0);
        billed.source = SOURCE_GLOBIRD;
        let days = [billed, day_cost_of("2026-09-02", 0.0)];
        let months = bucket_days(Group::Month, &days);
        assert_eq!(
            (months[0].totals.days, months[0].totals.billed_days),
            (2, 1)
        );
    }

    #[test]
    fn the_group_parameter_is_checked() {
        assert_eq!(Group::parse(None).unwrap(), Group::Day);
        assert_eq!(Group::parse(Some("day")).unwrap(), Group::Day);
        assert_eq!(Group::parse(Some("month")).unwrap(), Group::Month);
        assert_eq!(Group::parse(Some("year")).unwrap(), Group::Year);
        assert!(Group::parse(Some("week")).is_err());
    }

    #[test]
    fn days_are_summed_into_months_and_years() {
        let days = [
            day_cost_of("2025-12-31", 0.0),
            day_cost_of("2026-01-01", 100.0),
            day_cost_of("2026-01-02", 0.0),
            // February has no data: it gets no bucket.
            day_cost_of("2026-03-05", 100.0),
        ];
        let months = bucket_days(Group::Month, &days);
        let summary: Vec<_> = months
            .iter()
            .map(|b| (b.period.as_str(), b.totals.days, b.totals.credit_days))
            .collect();
        assert_eq!(
            summary,
            [("2025-12", 1, 0), ("2026-01", 2, 1), ("2026-03", 1, 1)]
        );
        assert_eq!(months[1].totals.supply, 2.0);

        let years = bucket_days(Group::Year, &days);
        let summary: Vec<_> = years
            .iter()
            .map(|b| (b.period.as_str(), b.totals.days, b.totals.credit_days))
            .collect();
        assert_eq!(summary, [("2025", 1, 0), ("2026", 3, 2)]);
        assert!(bucket_days(Group::Year, &[]).is_empty());
    }
}
