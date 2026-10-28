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
use super::costs::{SOURCE_BILL, SOURCE_GLOBIRD, billed_days, day_grid_split, live_in, round2};
use crate::bills::PastBill;
use crate::cost::{DayCost, GridSplit, Tariff};
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

/// The bill period that contains `today`.
///
/// 1. With `start_day` (1 to 28) the period starts on that day of a month and
///    runs to the day before the next one.
/// 2. Else the most recent bill that ended before `today` (`bills` is oldest
///    first): the period starts the day after it ended and is as long as it,
///    rolled forward by that length until it contains `today`.
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
    if let Some(last) = bills.iter().rev().find(|b| b.to < today) {
        let length = last.days().max(1);
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
    export_cents: f64,
    export_kwh: f64,
    credit_cents: f64,
    actual_cents: f64,
    baseline_cents: f64,
}

impl PeriodSums {
    fn add(&mut self, cost: &DayCost) {
        self.days += 1;
        if cost.zero_import_credit_cents > 0.0 {
            self.credit_days += 1;
        }
        self.supply_cents += cost.supply_cents;
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
    let mut sums = PeriodSums::default();
    let mut bands = [BandSum::default(); 3];
    let mut uses_globird = false;
    for day in &costed {
        sums.add(&day.cost);
        uses_globird |= day.source == SOURCE_GLOBIRD || day.source == SOURCE_BILL;
        let (start, end) = day_bounds_or_error(day.date, state.tz)?;
        let live = live_in(&state.pool, live_range, start, end).await?;
        if let Some(split) = day_grid_split(&state, schedule, day.date, &live, now).await? {
            let tariff = schedule.for_date(day.date);
            add_bands(&mut bands, &split, tariff, day.cost.import_cents);
        }
    }

    let supply_per_day = schedule.for_date(today).supply_cents_per_day();
    let (projected, projected_without) = project(&sums, days_total, supply_per_day);
    let names = ["free", "shoulder", "peak"];
    let bands: Vec<_> = names
        .iter()
        .zip(bands)
        .map(|(band, sum)| json!({ "band": band, "kwh": round2(sum.kwh), "dollars": dollars(sum.cents) }))
        .collect();
    Ok(Json(json!({
        "configured": true,
        "from": period.from.to_string(),
        "to": period.to.to_string(),
        "days_total": days_total,
        "days_elapsed": days_elapsed,
        "period_source": period.source,
        "so_far": {
            "supply_dollars": dollars(sums.supply_cents),
            "bands": bands,
            "feed_in_kwh": round2(sums.export_kwh),
            "feed_in_dollars": dollars(sums.export_cents),
            "credit_nights": sums.credit_days,
            "credit_dollars": dollars(sums.credit_cents),
            "total_dollars": dollars(sums.actual_cents),
            "without_system_dollars": dollars(sums.baseline_cents),
        },
        "projected_total_dollars": dollars(projected),
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
}
