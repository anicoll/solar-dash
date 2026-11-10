//! `GET /api/bill/current`: what the GloBird bill is so far in the current
//! bill period and where it is heading. It reuses the cost engine behind
//! `/api/costs` ([`billed_days`], so complete days GloBird has billed show
//! GloBird's own figures) and the band split behind `/api/daily`, so no tariff
//! maths lives here, only the period dates and the projection.

use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Days, Months, NaiveDate};
use serde::Serialize;
use serde_json::json;

use super::AppState;
use super::api::{ApiResult, day_bounds_or_error, today};
use super::costs::{
    CostedDay, SOURCE_BILL, SOURCE_GLOBIRD, billed_days, day_grid_split, hhmm, live_in, round2,
};
use crate::bills::PastBill;
use crate::cost::{DayCost, ExportBand, GridSplit, Tariff};
use crate::db;

/// How the period's dates were chosen, as `period_source` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PeriodSource {
    /// The admin setting `bill_start_day`.
    Setting,
    /// Rolled forward from the most recent past bill.
    LastBill,
    /// The calendar month.
    Month,
}

/// A bill period: inclusive local dates.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Period {
    from: NaiveDate,
    to: NaiveDate,
    source: PeriodSource,
}

impl Period {
    fn days_total(&self) -> i64 {
        (self.to - self.from).num_days() + 1
    }
}

/// The shortest last bill, in days, that is still used to roll a period
/// forward. A retailer's final bill can be a few days long, and rolling by
/// that would make every later period just as short.
const MIN_ROLLING_BILL_DAYS: i64 = 25;

/// The bill period that contains `today`.
///
/// 1. With `start_day` (1 to 28) the period starts on that day of a month and
///    runs to the day before the next one.
/// 2. Else the most recent bill that ended before `today` (`bills` is oldest
///    first): the period starts the day after it ended and is as long as it,
///    rolled forward by that length until it contains `today`. A bill shorter
///    than [`MIN_ROLLING_BILL_DAYS`] (such as a final bill) is not used.
/// 3. Else the calendar month.
fn current_period(today: NaiveDate, start_day: Option<u32>, bills: &[PastBill]) -> Period {
    if let Some(day) = start_day.filter(|d| (1..=28).contains(d)) {
        // Days 1 to 28 exist in every month, so building the date cannot fail.
        let this_month = today.with_day(day).unwrap_or(today);
        let from = if today.day() >= day {
            this_month
        } else {
            this_month - Months::new(1)
        };
        let to = (from + Months::new(1)) - Days::new(1);
        return Period {
            from,
            to,
            source: PeriodSource::Setting,
        };
    }
    if let Some(last) = bills
        .iter()
        .rev()
        .find(|b| b.to < today)
        .filter(|b| b.days() >= MIN_ROLLING_BILL_DAYS)
    {
        let length = last.days();
        let first = last.to + Days::new(1);
        // Whole periods that have passed since the one after the bill.
        let rolled = (today - first).num_days() / length;
        let from = first + Days::new((rolled * length) as u64);
        return Period {
            from,
            to: from + Days::new(length as u64 - 1),
            source: PeriodSource::LastBill,
        };
    }
    let from = today.with_day(1).unwrap_or(today);
    Period {
        from,
        to: (from + Months::new(1)) - Days::new(1),
        source: PeriodSource::Month,
    }
}

/// The money of the complete days so far, in unrounded cents.
#[derive(Debug, Default, Clone, PartialEq)]
struct PeriodSums {
    /// Days with a cost row.
    days: u32,
    credit_days: u32,
    supply_cents: f64,
    import_cents: f64,
    export_cents: f64,
    export_kwh: f64,
    credit_cents: f64,
    actual_cents: f64,
    baseline_cents: f64,
}

impl PeriodSums {
    /// The sums of the days that have a cost row.
    fn of(days: &[CostedDay]) -> Self {
        let mut sums = PeriodSums::default();
        for day in days {
            sums.add(&day.cost);
        }
        sums
    }

    fn add(&mut self, cost: &DayCost) {
        self.days += 1;
        if cost.zero_import_credit_cents > 0.0 {
            self.credit_days += 1;
        }
        self.supply_cents += cost.supply_cents;
        self.import_cents += cost.import_cents;
        self.export_cents += cost.export_cents;
        self.export_kwh += cost.export_kwh;
        self.credit_cents += cost.zero_import_credit_cents;
        self.actual_cents += cost.actual_cents;
        self.baseline_cents += cost.baseline_cents;
    }
}

/// Where the period is heading, as `(total, without_system)` in cents.
///
/// The method is deliberately simple. The average day so far (supply, import,
/// feed-in and credit, which together are `actual_cents`) is multiplied by the
/// days in the whole period, and so is the average no-system baseline. With no
/// complete day yet there is nothing to average: the total is the plan's
/// supply charge for every day (`fallback_supply_cents` per day) and
/// `without_system` is `None`.
fn project(sums: &PeriodSums, days_total: i64, fallback_supply_cents: f64) -> (f64, Option<f64>) {
    if sums.days == 0 {
        return (fallback_supply_cents * days_total as f64, None);
    }
    let scale = days_total as f64 / f64::from(sums.days);
    (sums.actual_cents * scale, Some(sums.baseline_cents * scale))
}

/// Grid import for one band: kWh and cents.
#[derive(Debug, Default, Clone, Copy)]
struct BandSum {
    kwh: f64,
    cents: f64,
}

/// Adds one day's band split to `bands` (free, shoulder, peak). Each band's
/// kWh comes from `split`. Its cents are the band's usual price times its kWh,
/// scaled so the three add up to the day's real `import_cents`, which keeps
/// the bands in step with GloBird's bill on the days that have one.
fn add_bands(bands: &mut [BandSum; 3], split: &GridSplit, tariff: &Tariff, import_cents: f64) {
    let kwh = [
        split.import_free_kwh,
        split.import_shoulder_kwh,
        split.import_peak_kwh,
    ];
    let priced = [
        0.0,
        kwh[1] * tariff.default_import_cents(),
        kwh[2] * tariff.peak_import_cents(),
    ];
    let priced_total: f64 = priced.iter().sum();
    let scale = if priced_total > 0.0 {
        import_cents / priced_total
    } else {
        0.0
    };
    for i in 0..3 {
        bands[i].kwh += kwh[i];
        bands[i].cents += priced[i] * scale;
    }
}

/// A feed-in window of a tariff: local minutes since midnight and its price.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ExportRate {
    from: u32,
    to: u32,
    cents: f64,
}

/// The feed-in prices of one tariff, by role (see [`Tariff::export_band_for`]).
/// The super window is the best-paid one (only when a lower better-than-default
/// one exists too) and the peak window is the other better-than-default one,
/// normally the wider of the two. Either is `None` when the plan has no such
/// window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ExportRates {
    super_window: Option<ExportRate>,
    peak_window: Option<ExportRate>,
    other_cents: f64,
    tier_cents: Option<f64>,
}

impl ExportRates {
    fn of(tariff: &Tariff) -> Self {
        let mut rates = ExportRates {
            super_window: None,
            peak_window: None,
            other_cents: tariff.default_export_cents(),
            tier_cents: tariff.export_tier().map(|(_, cents)| cents),
        };
        // The first window of a role wins, as the first match wins when pricing.
        for (from, to, cents) in tariff.export_windows() {
            let rate = ExportRate { from, to, cents };
            match tariff.export_band_for(cents) {
                ExportBand::Super => rates.super_window = rates.super_window.or(Some(rate)),
                ExportBand::Peak => rates.peak_window = rates.peak_window.or(Some(rate)),
                ExportBand::Other => {}
            }
        }
        rates
    }

    /// The price in cents of each part of a [`GridSplit`]'s export, in the
    /// order of [`FEED_IN_WINDOWS`]. A part the plan has no price for is 0.
    fn cents(&self) -> [f64; 4] {
        [
            self.super_window.map_or(0.0, |w| w.cents),
            self.peak_window.map_or(0.0, |w| w.cents),
            self.other_cents,
            self.tier_cents.unwrap_or(0.0),
        ]
    }
}

/// The feed-in windows as `feed_in_windows` reports them, in this order.
const FEED_IN_WINDOWS: [&str; 4] = ["super", "peak", "other", "tier"];

/// Adds one day's feed-in split to `windows` (super, peak, other, tier). Like
/// [`add_bands`], each part's cents are its usual price times its kWh, scaled
/// so the four add up to the day's real `export_cents`.
fn add_feed_in(windows: &mut [BandSum; 4], split: &GridSplit, tariff: &Tariff, export_cents: f64) {
    let kwh = [
        split.export_super_kwh,
        split.export_peak_kwh,
        split.export_other_kwh,
        split.export_tier_kwh,
    ];
    let price = ExportRates::of(tariff).cents();
    let priced: Vec<f64> = kwh.iter().zip(price).map(|(k, p)| k * p).collect();
    let priced_total: f64 = priced.iter().sum();
    let scale = if priced_total > 0.0 {
        export_cents / priced_total
    } else {
        0.0
    };
    for i in 0..4 {
        windows[i].kwh += kwh[i];
        windows[i].cents += priced[i] * scale;
    }
}

/// One feed-in window on the bill, as `so_far.feed_in_windows` lists it.
#[derive(Debug, PartialEq, Serialize)]
struct FeedInWindow {
    /// "super", "peak", "other" or "tier".
    window: &'static str,
    /// Local "HH:MM" start and end; `null` for `other` and `tier`, which have
    /// no window of their own.
    from: Option<String>,
    to: Option<String>,
    kwh: f64,
    /// The window's price now; `null` when the current plan has no such window.
    cents_per_kwh: Option<f64>,
    dollars: f64,
}

/// The windows to report: the four parts of `sums` with the times and prices
/// of `rates`. A part with no energy and no money is left out (so `tier` only
/// shows when the plan has paid it).
fn feed_in_windows(sums: &[BandSum; 4], rates: &ExportRates) -> Vec<FeedInWindow> {
    let times = [rates.super_window, rates.peak_window, None, None];
    let cents = [
        rates.super_window.map(|w| w.cents),
        rates.peak_window.map(|w| w.cents),
        Some(rates.other_cents),
        rates.tier_cents,
    ];
    let mut out = Vec::new();
    for i in 0..4 {
        let sum = sums[i];
        if sum.kwh <= 0.0 && dollars(sum.cents) == 0.0 {
            continue;
        }
        out.push(FeedInWindow {
            window: FEED_IN_WINDOWS[i],
            from: times[i].map(|w| hhmm(w.from)),
            to: times[i].map(|w| hhmm(w.to)),
            kwh: round2(sum.kwh),
            cents_per_kwh: cents[i],
            dollars: dollars(sum.cents),
        });
    }
    out
}

/// How a day went for the zero-import credit: `None` when its plan has no
/// such credit, else "earned" (some credit paid) or "missed".
fn credit_status(plan_has_credit: bool, credit_cents: f64) -> Option<&'static str> {
    if !plan_has_credit {
        return None;
    }
    Some(if credit_cents > 0.0 {
        "earned"
    } else {
        "missed"
    })
}

/// One complete day in the `daily` series.
#[derive(Debug, PartialEq, Serialize)]
struct DailyEntry {
    date: String,
    total_dollars: f64,
    without_system_dollars: f64,
    credit: Option<&'static str>,
    source: &'static str,
}

/// A finished bill period in the `previous` field: the sums of its days that
/// have a cost row.
#[derive(Debug, PartialEq, Serialize)]
struct PreviousPeriod {
    from: String,
    to: String,
    days_total: i64,
    /// Days with a cost row.
    days: u32,
    supply_dollars: f64,
    import_dollars: f64,
    feed_in_dollars: f64,
    credit_dollars: f64,
    credit_nights: u32,
    total_dollars: f64,
    without_system_dollars: f64,
}

impl PreviousPeriod {
    /// `None` when no day of the period has a cost row.
    fn new(period: &Period, sums: &PeriodSums) -> Option<Self> {
        (sums.days > 0).then(|| PreviousPeriod {
            from: period.from.to_string(),
            to: period.to.to_string(),
            days_total: period.days_total(),
            days: sums.days,
            supply_dollars: dollars(sums.supply_cents),
            import_dollars: dollars(sums.import_cents),
            feed_in_dollars: dollars(sums.export_cents),
            credit_dollars: dollars(sums.credit_cents),
            credit_nights: sums.credit_days,
            total_dollars: dollars(sums.actual_cents),
            without_system_dollars: dollars(sums.baseline_cents),
        })
    }
}

/// One past period in `history`.
#[derive(Debug, PartialEq, Serialize)]
struct HistoryEntry {
    from: String,
    to: String,
    days_total: i64,
    days: u32,
    total_dollars: f64,
    without_system_dollars: f64,
}

impl From<&PreviousPeriod> for HistoryEntry {
    fn from(p: &PreviousPeriod) -> Self {
        HistoryEntry {
            from: p.from.clone(),
            to: p.to.clone(),
            days_total: p.days_total,
            days: p.days,
            total_dollars: p.total_dollars,
            without_system_dollars: p.without_system_dollars,
        }
    }
}

/// The cars' charging on this bill, as `cars` reports it.
#[derive(Debug, PartialEq, Serialize)]
struct CarsOnBill {
    sessions: i64,
    kwh: f64,
    grid_kwh: f64,
    self_kwh: f64,
    cost_dollars: f64,
}

impl CarsOnBill {
    fn new(totals: &db::TmChargeTotals) -> Self {
        CarsOnBill {
            sessions: totals.sessions,
            kwh: round2(totals.energy_used_kwh),
            grid_kwh: round2(totals.grid_kwh),
            self_kwh: round2(totals.self_kwh),
            cost_dollars: dollars(totals.cost_cents),
        }
    }
}

/// How many past periods `history` goes back.
const HISTORY_PERIODS: usize = 6;

/// The `count` periods before `period`, oldest first, each found the way the
/// current one is: the period that contains the day before the next one starts.
fn periods_before(
    period: &Period,
    count: usize,
    start_day: Option<u32>,
    bills: &[PastBill],
) -> Vec<Period> {
    let mut out = Vec::with_capacity(count);
    let mut next_from = period.from;
    for _ in 0..count {
        let before = current_period(next_from - Days::new(1), start_day, bills);
        next_from = before.from;
        out.push(before);
    }
    out.reverse();
    out
}

/// Sums each of `periods` from `costed` (any days, in any order; a day counts
/// in the period whose dates contain it).
fn sum_periods(periods: &[Period], costed: &[CostedDay]) -> Vec<PeriodSums> {
    periods
        .iter()
        .map(|p| {
            let days: Vec<CostedDay> = costed
                .iter()
                .filter(|d| p.from <= d.date && d.date <= p.to)
                .cloned()
                .collect();
            PeriodSums::of(&days)
        })
        .collect()
}

/// The projection as a range `(low, high)` in cents. `projected` is the usual
/// projection; the other end carries on at the previous period's average day:
/// `so_far_cents` plus `days_left` days of it. Without a previous period both
/// ends are `projected`.
fn projection_range(
    projected: f64,
    so_far_cents: f64,
    days_left: i64,
    previous: Option<&PeriodSums>,
) -> (f64, f64) {
    let Some(prev) = previous.filter(|p| p.days > 0) else {
        return (projected, projected);
    };
    let at_last_bills_pace =
        so_far_cents + days_left.max(0) as f64 * prev.actual_cents / f64::from(prev.days);
    (
        projected.min(at_last_bills_pace),
        projected.max(at_last_bills_pace),
    )
}

fn dollars(cents: f64) -> f64 {
    round2(cents / 100.0)
}

/// `GET /api/bill/current`. Only complete days count (today is left out), so
/// the figures do not move during the day. The bands cover the days that have
/// interval data; a day with only a past bill's figures adds to the dollar
/// totals but not to the bands.
pub async fn current(State(state): State<AppState>) -> ApiResult {
    let data = state.admin.get();
    let Some(schedule) = &data.tariff else {
        return Ok(Json(json!({ "configured": false })));
    };
    let today = today(state.tz);
    let period = current_period(today, data.bill_start_day, &data.bills);
    let days_total = period.days_total();

    // Complete days: from the period start to yesterday, and never past its end.
    let last_done = (today - Days::new(1)).min(period.to);
    let dates: Vec<NaiveDate> = period
        .from
        .iter_days()
        .take_while(|d| *d <= last_done)
        .collect();
    let days_elapsed = dates.len() as i64;
    let costed = billed_days(&state, schedule, &dates).await?;

    let live_range = db::live_ts_range(&state.pool).await?;
    let now = db::now();
    let sums = PeriodSums::of(&costed);
    let mut bands = [BandSum::default(); 3];
    let mut feed_in = [BandSum::default(); 4];
    let mut free_value_cents = 0.0;
    let mut credit_possible = 0u32;
    let mut daily = Vec::with_capacity(costed.len());
    let mut uses_globird = false;
    for day in &costed {
        uses_globird |= day.source == SOURCE_GLOBIRD || day.source == SOURCE_BILL;
        let has_credit = schedule.for_date(day.date).credit_cents().is_some();
        credit_possible += u32::from(has_credit);
        daily.push(DailyEntry {
            date: day.date.to_string(),
            total_dollars: dollars(day.cost.actual_cents),
            without_system_dollars: dollars(day.cost.baseline_cents),
            credit: credit_status(has_credit, day.cost.zero_import_credit_cents),
            source: day.source,
        });
        let (start, end) = day_bounds_or_error(day.date, state.tz)?;
        let live = live_in(&state.pool, live_range, start, end).await?;
        if let Some(split) = day_grid_split(&state, schedule, day.date, &live, now).await? {
            let tariff = schedule.for_date(day.date);
            add_bands(&mut bands, &split, tariff, day.cost.import_cents);
            add_feed_in(&mut feed_in, &split, tariff, day.cost.export_cents);
            free_value_cents += split.import_free_kwh * tariff.default_import_cents();
        }
    }

    let supply_per_day = schedule.for_date(today).supply_cents_per_day();
    let (projected, projected_without) = project(&sums, days_total, supply_per_day);
    // Rates and window times are the last complete day's plan (today's when
    // there is none yet).
    let rate_day = costed.last().map_or(today, |day| day.date);
    let rate_plan = schedule.for_date(rate_day);
    let band_cents = [
        0.0,
        rate_plan.default_import_cents(),
        rate_plan.peak_import_cents(),
    ];
    let names = ["free", "shoulder", "peak"];
    let bands: Vec<_> = names
        .iter()
        .zip(bands)
        .zip(band_cents)
        .map(|((band, sum), cents)| {
            json!({
                "band": band,
                "kwh": round2(sum.kwh),
                "dollars": dollars(sum.cents),
                "cents_per_kwh": cents,
            })
        })
        .collect();
    // The last six periods in one pass over the costs, oldest first.
    let before = periods_before(&period, HISTORY_PERIODS, data.bill_start_day, &data.bills);
    let past_dates: Vec<NaiveDate> = before.first().map_or_else(Vec::new, |first| {
        first
            .from
            .iter_days()
            .take_while(|d| *d < period.from)
            .collect()
    });
    let past_costed = billed_days(&state, schedule, &past_dates).await?;
    let past_sums = sum_periods(&before, &past_costed);
    let history: Vec<PreviousPeriod> = before
        .iter()
        .zip(&past_sums)
        .filter_map(|(p, s)| PreviousPeriod::new(p, s))
        .collect();
    let previous = before
        .last()
        .zip(past_sums.last())
        .and_then(|(p, s)| PreviousPeriod::new(p, s));
    let (projected_low, projected_high) = projection_range(
        projected,
        sums.actual_cents,
        days_total - days_elapsed,
        past_sums.last(),
    );
    let history: Vec<HistoryEntry> = history.iter().map(HistoryEntry::from).collect();
    // Charging sessions that started on a complete day of this period.
    let cars = match dates.first().zip(dates.last()) {
        Some((first, last)) => {
            let (start, _) = day_bounds_or_error(*first, state.tz)?;
            let (_, end) = day_bounds_or_error(*last, state.tz)?;
            db::tm_charge_totals(&state.pool, start, end).await?
        }
        None => None,
    };
    let cars = cars.as_ref().map(CarsOnBill::new);
    let feed_in_windows = feed_in_windows(&feed_in, &ExportRates::of(rate_plan));
    Ok(Json(json!({
        "configured": true,
        "from": period.from.to_string(),
        "to": period.to.to_string(),
        "days_total": days_total,
        "days_elapsed": days_elapsed,
        "period_source": period.source,
        "so_far": {
            "supply_dollars": dollars(sums.supply_cents),
            "supply_days": sums.days,
            "supply_cents_per_day": rate_plan.supply_cents_per_day(),
            "bands": bands,
            "free_value_dollars": dollars(free_value_cents),
            "feed_in_windows": feed_in_windows,
            "credit_possible_nights": credit_possible,
            "feed_in_kwh": round2(sums.export_kwh),
            "feed_in_dollars": dollars(sums.export_cents),
            "credit_nights": sums.credit_days,
            "credit_dollars": dollars(sums.credit_cents),
            "total_dollars": dollars(sums.actual_cents),
            "without_system_dollars": dollars(sums.baseline_cents),
        },
        "daily": daily,
        "previous": previous,
        "projected_total_dollars": dollars(projected),
        "projected_low_dollars": dollars(projected_low),
        "projected_high_dollars": dollars(projected_high),
        "history": history,
        "cars": cars,
        "projected_without_system_dollars": projected_without.map(dollars),
        "uses_globird_figures": uses_globird,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn bill(from: &str, to: &str) -> PastBill {
        PastBill {
            from: date(from),
            to: date(to),
            bill_total: 0.0,
            excluded: 0.0,
            import_kwh: 0.0,
            import_dollars: 0.0,
            export_kwh: 0.0,
            export_dollars: 0.0,
            supply_dollars: 0.0,
            note: String::new(),
        }
    }

    fn span(period: Period) -> (String, String, PeriodSource) {
        (
            period.from.to_string(),
            period.to.to_string(),
            period.source,
        )
    }

    #[test]
    fn setting_starts_on_that_day_of_this_or_last_month() {
        // On or after the start day: this month's start.
        let p = current_period(date("2026-10-28"), Some(15), &[]);
        assert_eq!(
            span(p),
            (
                "2026-10-15".into(),
                "2026-11-14".into(),
                PeriodSource::Setting
            )
        );
        assert_eq!(p.days_total(), 31);
        let p = current_period(date("2026-10-15"), Some(15), &[]);
        assert_eq!(p.from, date("2026-10-15"));
        // Before it: last month's start.
        let p = current_period(date("2026-10-14"), Some(15), &[]);
        assert_eq!((p.from, p.to), (date("2026-09-15"), date("2026-10-14")));
        // Across a year end.
        let p = current_period(date("2027-01-03"), Some(15), &[]);
        assert_eq!((p.from, p.to), (date("2026-12-15"), date("2027-01-14")));
        // The setting wins over a bill.
        let bills = [bill("2026-09-01", "2026-09-30")];
        let p = current_period(date("2026-10-28"), Some(1), &bills);
        assert_eq!(p.source, PeriodSource::Setting);
    }

    #[test]
    fn setting_handles_the_28th_and_february() {
        let p = current_period(date("2027-02-10"), Some(28), &[]);
        assert_eq!((p.from, p.to), (date("2027-01-28"), date("2027-02-27")));
        let p = current_period(date("2027-02-28"), Some(28), &[]);
        assert_eq!((p.from, p.to), (date("2027-02-28"), date("2027-03-27")));
        assert_eq!(p.days_total(), 28);
        // Leap year: February has 29 days.
        let p = current_period(date("2028-03-01"), Some(28), &[]);
        assert_eq!((p.from, p.to), (date("2028-02-28"), date("2028-03-27")));
        assert_eq!(p.days_total(), 29);
        // Start day 1 is the calendar month.
        let p = current_period(date("2027-02-14"), Some(1), &[]);
        assert_eq!((p.from, p.to), (date("2027-02-01"), date("2027-02-28")));
    }

    #[test]
    fn short_last_bill_falls_back_to_the_calendar_month() {
        // A 7 day final bill is not used for rolling.
        let bills = [bill("2026-05-19", "2026-05-25")];
        let p = current_period(date("2026-11-10"), None, &bills);
        assert_eq!(
            span(p),
            (
                "2026-11-01".into(),
                "2026-11-30".into(),
                PeriodSource::Month
            )
        );
        // 24 days is too short, 25 is the shortest that still rolls.
        let bills = [bill("2026-09-01", "2026-09-24")];
        let p = current_period(date("2026-10-10"), None, &bills);
        assert_eq!(p.source, PeriodSource::Month);
        let bills = [bill("2026-09-01", "2026-09-25")];
        let p = current_period(date("2026-10-10"), None, &bills);
        assert_eq!(p.source, PeriodSource::LastBill);
    }

    #[test]
    fn a_28_day_last_bill_still_rolls() {
        let bills = [bill("2026-09-01", "2026-09-28")];
        let p = current_period(date("2026-10-10"), None, &bills);
        assert_eq!(
            span(p),
            (
                "2026-09-29".into(),
                "2026-10-26".into(),
                PeriodSource::LastBill
            )
        );
    }

    #[test]
    fn last_bill_rolls_forward_by_its_length() {
        // A 31 day bill ending 2026-09-14: the next period starts 09-15.
        let bills = [bill("2026-08-15", "2026-09-14")];
        let p = current_period(date("2026-10-10"), None, &bills);
        assert_eq!(
            span(p),
            (
                "2026-09-15".into(),
                "2026-10-15".into(),
                PeriodSource::LastBill
            )
        );
        // Past the end of that one, it rolls to the next.
        let p = current_period(date("2026-10-28"), None, &bills);
        assert_eq!((p.from, p.to), (date("2026-10-16"), date("2026-11-15")));
        assert_eq!(p.days_total(), 31);
        // The most recent bill is used, and one that ends today is not "past".
        let bills = [
            bill("2026-08-15", "2026-09-14"),
            bill("2026-09-15", "2026-10-14"),
        ];
        let p = current_period(date("2026-10-15"), None, &bills);
        assert_eq!(p.from, date("2026-10-15"));
        let p = current_period(date("2026-10-14"), None, &bills);
        assert_eq!(p.from, date("2026-09-15"));
    }

    #[test]
    fn without_a_setting_or_bill_it_is_the_calendar_month() {
        let p = current_period(date("2026-10-28"), None, &[]);
        assert_eq!(
            span(p),
            (
                "2026-10-01".into(),
                "2026-10-31".into(),
                PeriodSource::Month
            )
        );
        let p = current_period(date("2028-02-29"), None, &[]);
        assert_eq!((p.from, p.to), (date("2028-02-01"), date("2028-02-29")));
        assert_eq!(p.days_total(), 29);
    }

    #[test]
    fn projection_scales_the_daily_average_to_the_whole_period() {
        let mut sums = PeriodSums::default();
        // Two days: supply 100c, import 300c, feed-in 100c, credit 100c each.
        for _ in 0..2 {
            sums.add(&DayCost {
                import_kwh: 0.0,
                export_kwh: 5.0,
                load_kwh: 0.0,
                import_cents: 300.0,
                export_cents: 100.0,
                supply_cents: 100.0,
                zero_import_credit_cents: 100.0,
                actual_cents: 200.0,
                baseline_cents: 1000.0,
                savings_cents: 800.0,
            });
        }
        assert_eq!((sums.days, sums.credit_days), (2, 2));
        let (total, without) = project(&sums, 30, 99.0);
        assert_eq!(total, 6000.0);
        assert_eq!(without, Some(30_000.0));
    }

    #[test]
    fn projection_with_no_days_is_just_supply() {
        let (total, without) = project(&PeriodSums::default(), 31, 100.0);
        assert_eq!(total, 3100.0);
        assert_eq!(without, None);
    }

    /// A plan with a 10 c "super" window inside a wider 4 c "peak" one, a
    /// 1 c default and, if asked, a daily tier.
    fn plan(tier: bool) -> Tariff {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let tier = if tier {
            "export_tier = { first_kwh_per_day = 5.0, cents = 20.0 }"
        } else {
            ""
        };
        let text = format!(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 30.0
            default_export_cents = 1.0
            import = [{{ from = "00:00", to = "03:00", cents = 0.0 }}]
            export = [
              {{ from = "18:00", to = "20:00", cents = 10.0 }},
              {{ from = "16:00", to = "21:00", cents = 4.0 }},
            ]
            {tier}
            "#
        );
        let w: Wrapper = toml::from_str(&text).unwrap();
        Tariff::from_config(&w.tariff).unwrap()
    }

    fn kwh_split(sup: f64, peak: f64, other: f64, tier: f64) -> GridSplit {
        GridSplit {
            export_super_kwh: sup,
            export_peak_kwh: peak,
            export_other_kwh: other,
            export_tier_kwh: tier,
            ..GridSplit::default()
        }
    }

    #[test]
    fn export_rates_tell_the_super_window_from_the_wider_one() {
        let rates = ExportRates::of(&plan(true));
        assert_eq!(
            rates.super_window,
            Some(ExportRate {
                from: 18 * 60,
                to: 20 * 60,
                cents: 10.0
            })
        );
        assert_eq!(
            rates.peak_window,
            Some(ExportRate {
                from: 16 * 60,
                to: 21 * 60,
                cents: 4.0
            })
        );
        assert_eq!(rates.cents(), [10.0, 4.0, 1.0, 20.0]);
        assert_eq!(ExportRates::of(&plan(false)).tier_cents, None);
    }

    #[test]
    fn feed_in_parts_add_up_to_the_days_export_money() {
        let tariff = plan(false);
        let mut windows = [BandSum::default(); 4];
        // 2 kWh super (20c), 5 kWh peak (20c), 10 kWh other (10c) = 50c priced,
        // but the day really earned 100c: every part doubles.
        add_feed_in(
            &mut windows,
            &kwh_split(2.0, 5.0, 10.0, 0.0),
            &tariff,
            100.0,
        );
        let cents: Vec<f64> = windows.iter().map(|w| w.cents).collect();
        assert_eq!(cents, vec![40.0, 40.0, 20.0, 0.0]);
        // A day with no priced export adds energy but no money.
        let mut none = [BandSum::default(); 4];
        add_feed_in(&mut none, &kwh_split(0.0, 0.0, 3.0, 0.0), &tariff, 0.0);
        assert_eq!((none[2].kwh, none[2].cents), (3.0, 0.0));
    }

    #[test]
    fn feed_in_windows_list_in_order_and_skip_empty_ones() {
        let rates = ExportRates::of(&plan(true));
        let part = |kwh, cents| BandSum { kwh, cents };
        let sums = [
            part(2.0, 40.0),
            BandSum::default(),
            part(10.0, 20.0),
            BandSum::default(),
        ];
        let out = feed_in_windows(&sums, &rates);
        let names: Vec<_> = out.iter().map(|w| w.window).collect();
        assert_eq!(names, ["super", "other"]);
        assert_eq!(out[0].from.as_deref(), Some("18:00"));
        assert_eq!(out[0].to.as_deref(), Some("20:00"));
        assert_eq!(out[0].cents_per_kwh, Some(10.0));
        assert_eq!(out[0].dollars, 0.4);
        assert_eq!(out[1].from, None);
        assert_eq!(out[1].cents_per_kwh, Some(1.0));
        // The tier shows once it has energy.
        let mut all = sums;
        all[3] = part(5.0, 100.0);
        let out = feed_in_windows(&all, &rates);
        let last = out.last().unwrap();
        assert_eq!((last.window, last.cents_per_kwh), ("tier", Some(20.0)));
        // A plan without the window reports no times or price for it.
        let flat = ExportRates {
            super_window: None,
            peak_window: None,
            other_cents: 1.0,
            tier_cents: None,
        };
        let out = feed_in_windows(&sums, &flat);
        assert_eq!((out[0].from.clone(), out[0].cents_per_kwh), (None, None));
    }

    #[test]
    fn credit_status_needs_a_credit_in_the_plan() {
        assert_eq!(credit_status(false, 0.0), None);
        assert_eq!(credit_status(false, 100.0), None);
        assert_eq!(credit_status(true, 100.0), Some("earned"));
        assert_eq!(credit_status(true, 0.0), Some("missed"));
    }

    fn day_cost(actual: f64, credit: f64) -> DayCost {
        DayCost {
            import_kwh: 0.0,
            export_kwh: 1.0,
            load_kwh: 0.0,
            import_cents: 300.0,
            export_cents: 100.0,
            supply_cents: 100.0,
            zero_import_credit_cents: credit,
            actual_cents: actual,
            baseline_cents: 1000.0,
            savings_cents: 0.0,
        }
    }

    fn costed(day: &str, actual: f64, credit: f64) -> CostedDay {
        CostedDay {
            date: date(day),
            source: "inverter",
            cost: day_cost(actual, credit),
        }
    }

    #[test]
    fn cars_on_bill_are_rounded_to_cents_and_hundredths() {
        let totals = db::TmChargeTotals {
            sessions: 4,
            energy_used_kwh: 52.456,
            grid_kwh: 30.004,
            self_kwh: 22.452,
            cost_cents: 1234.5,
        };
        assert_eq!(
            CarsOnBill::new(&totals),
            CarsOnBill {
                sessions: 4,
                kwh: 52.46,
                grid_kwh: 30.0,
                self_kwh: 22.45,
                cost_dollars: 12.35,
            }
        );
    }

    #[test]
    fn periods_before_walk_back_one_period_at_a_time() {
        let current = current_period(date("2026-11-10"), Some(5), &[]);
        let before = periods_before(&current, 3, Some(5), &[]);
        let spans: Vec<_> = before
            .iter()
            .map(|p| (p.from.to_string(), p.to.to_string()))
            .collect();
        assert_eq!(
            spans,
            [
                ("2026-08-05".to_string(), "2026-09-04".to_string()),
                ("2026-09-05".to_string(), "2026-10-04".to_string()),
                ("2026-10-05".to_string(), "2026-11-04".to_string()),
            ]
        );
        assert!(periods_before(&current, 0, Some(5), &[]).is_empty());
    }

    #[test]
    fn periods_are_summed_from_their_own_days() {
        let current = current_period(date("2026-11-10"), Some(5), &[]);
        let before = periods_before(&current, 2, Some(5), &[]);
        let days = [
            costed("2026-09-20", 500.0, 0.0),
            costed("2026-10-04", 300.0, 100.0),
            costed("2026-10-06", 200.0, 0.0),
        ];
        let sums = sum_periods(&before, &days);
        assert_eq!((sums[0].days, sums[0].actual_cents), (2, 800.0));
        assert_eq!((sums[1].days, sums[1].credit_days), (1, 0));
        let first = PreviousPeriod::new(&before[0], &sums[0]).unwrap();
        assert_eq!(
            (first.from.as_str(), first.days_total, first.days),
            ("2026-09-05", 30, 2)
        );
        assert_eq!(
            (
                first.total_dollars,
                first.import_dollars,
                first.credit_nights
            ),
            (8.0, 6.0, 1)
        );
        // A period with no costed day has no summary.
        assert_eq!(
            PreviousPeriod::new(&before[0], &PeriodSums::default()),
            None
        );
    }

    #[test]
    fn projection_range_spans_this_pace_and_last_bills_pace() {
        let mut prev = PeriodSums::default();
        for day in ["2026-10-01", "2026-10-02"] {
            prev.add(&costed(day, 400.0, 0.0).cost);
        }
        // Last bill ran 400c a day; 10 days left on 1000c so far is 5000c.
        assert_eq!(
            projection_range(7000.0, 1000.0, 10, Some(&prev)),
            (5000.0, 7000.0)
        );
        assert_eq!(
            projection_range(3000.0, 1000.0, 10, Some(&prev)),
            (3000.0, 5000.0)
        );
        // Nothing left, or no previous period: no range.
        assert_eq!(
            projection_range(900.0, 1000.0, 0, Some(&prev)),
            (900.0, 1000.0)
        );
        assert_eq!(projection_range(900.0, 1000.0, 10, None), (900.0, 900.0));
        let empty = PeriodSums::default();
        assert_eq!(
            projection_range(900.0, 1000.0, 10, Some(&empty)),
            (900.0, 900.0)
        );
    }
}
