//! `/api/payback`: how much of what the system cost has been saved so far,
//! and when it should be paid back.
//!
//! The savings come from [`billed_days`]: our cached estimate, with GloBird's
//! own figures put in for the days it has billed. The
//! maths below is pure and works on plain `(date, dollars)` pairs, so it is
//! tested with plain numbers.

use std::collections::BTreeMap;

use anyhow::Result;
use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Days, Months, NaiveDate};
use serde::Serialize;
use serde_json::{Value, json};

use super::AppState;
use super::api::{ApiResult, today};
use super::costs::{billed_days, round2};
use crate::config;

/// How many days of savings the rate is averaged over.
const RATE_DAYS: usize = 365;
/// The furthest back savings are counted. It stops a mistyped purchase year
/// from asking for millions of days. About 11 years.
const MAX_PAYBACK_DAYS: u64 = 4000;
/// A payback date further away than this (about 100 years) is reported as "never".
const MAX_PROJECTED_DAYS: f64 = 36_500.0;
/// Mean length of a year, for turning the yearly price rise into days.
const DAYS_PER_YEAR: f64 = 365.25;

/// One thing bought for the system, with its date already parsed.
#[derive(Debug, Clone)]
pub struct Purchase {
    pub name: String,
    pub date: NaiveDate,
    pub cost_dollars: f64,
}

/// Convert the config's purchases, oldest first. `Config::load` already
/// checks them, so an error here means the config was built some other way.
pub fn purchases_from_config(list: &[config::Purchase]) -> Result<Vec<Purchase>> {
    let mut purchases = list
        .iter()
        .map(|p| {
            Ok(Purchase {
                name: p.name.clone(),
                date: config::parse_date(&p.date)?,
                cost_dollars: p.cost_dollars,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    purchases.sort_by_key(|p| p.date);
    Ok(purchases)
}

/// Savings in dollars on one local day.
type DaySavings = (NaiveDate, f64);

/// The end of a month: cumulative savings and the net position (savings
/// minus what had been paid by then).
#[derive(Debug, PartialEq, Serialize)]
struct MonthPoint {
    /// "YYYY-MM".
    month: String,
    saved: f64,
    net: f64,
}

/// One calendar year of savings.
#[derive(Debug, PartialEq, Serialize)]
struct YearTotal {
    year: i32,
    saved: f64,
    /// Days with data in this year.
    days: u32,
    /// The year has fewer days of data than days in the year.
    partial: bool,
}

/// When the system pays for itself.
#[derive(Debug, PartialEq)]
enum Projection {
    /// Savings already cover the cost.
    PaidBack,
    /// The savings rate is not positive, so it never will.
    Never,
    /// At the current rate, on about this day.
    On(NaiveDate),
}

/// The last day of the month `date` is in.
fn month_end(date: NaiveDate) -> NaiveDate {
    date.with_day(1)
        .and_then(|first| first.checked_add_months(Months::new(1)))
        .and_then(|next| next.pred_opt())
        .unwrap_or(date)
}

/// One point per month from `first`'s month to `last`'s: savings so far and
/// the net position at the end of that month, where the net subtracts every
/// purchase dated on or before that month's end. `days` must be oldest first.
fn monthly_series(
    days: &[DaySavings],
    purchases: &[Purchase],
    first: NaiveDate,
    last: NaiveDate,
) -> Vec<MonthPoint> {
    let mut points = Vec::new();
    let (mut saved, mut next_day) = (0.0, 0);
    let mut month_start = first.with_day(1);
    while let Some(start) = month_start
        && start <= last
    {
        let end = month_end(start);
        // Days are sorted, so each month continues where the last one stopped.
        while next_day < days.len() && days[next_day].0 <= end {
            saved += days[next_day].1;
            next_day += 1;
        }
        let spent: f64 = purchases
            .iter()
            .filter(|p| p.date <= end)
            .map(|p| p.cost_dollars)
            .sum();
        points.push(MonthPoint {
            month: start.format("%Y-%m").to_string(),
            saved: round2(saved),
            net: round2(saved - spent),
        });
        month_start = start.checked_add_months(Months::new(1));
    }
    points
}

/// Mean savings per day over the last [`RATE_DAYS`] days that have data (all
/// of them if there are fewer). 0 with no data. `days` must be oldest first.
fn rate_per_day(days: &[DaySavings]) -> f64 {
    let recent = &days[days.len().saturating_sub(RATE_DAYS)..];
    if recent.is_empty() {
        return 0.0;
    }
    recent.iter().map(|d| d.1).sum::<f64>() / recent.len() as f64
}

/// When `net` (savings minus cost, so negative until paid back) reaches zero
/// if savings keep coming at `rate` dollars a day from `today`, growing
/// continuously by `price_rise_percent` a year. With g = ln(1 + p/100), the
/// savings after T years are rate * 365.25 * (e^(gT) - 1) / g; solving that
/// for the remaining cost gives T = ln(1 + remaining * g / (rate * 365.25)) / g.
fn projected_payback(net: f64, rate: f64, price_rise_percent: f64, today: NaiveDate) -> Projection {
    if net >= 0.0 {
        return Projection::PaidBack;
    }
    if rate <= 0.0 {
        return Projection::Never;
    }
    let remaining = -net;
    let growth = (1.0 + price_rise_percent / 100.0).ln();
    let days = if growth > 0.0 {
        (1.0 + remaining * growth / (rate * DAYS_PER_YEAR)).ln() / growth * DAYS_PER_YEAR
    } else {
        remaining / rate
    }
    .ceil();
    if days > MAX_PROJECTED_DAYS {
        return Projection::Never;
    }
    match today.checked_add_days(Days::new(days as u64)) {
        Some(date) => Projection::On(date),
        None => Projection::Never,
    }
}

/// Days in a calendar year: 365, or 366 in a leap year.
fn days_in_year(year: i32) -> u32 {
    NaiveDate::from_ymd_opt(year, 12, 31).map_or(365, |last| last.ordinal())
}

/// Savings, days with data and whether the data is partial, per year, oldest
/// first. Years with no data are left out.
fn by_year(days: &[DaySavings]) -> Vec<YearTotal> {
    // A `BTreeMap` keeps its keys sorted, so the years come out in order.
    let mut years: BTreeMap<i32, (f64, u32)> = BTreeMap::new();
    for &(date, savings) in days {
        let entry = years.entry(date.year()).or_default();
        entry.0 += savings;
        entry.1 += 1;
    }
    years
        .into_iter()
        .map(|(year, (saved, days))| YearTotal {
            year,
            saved: round2(saved),
            days,
            partial: days < days_in_year(year),
        })
        .collect()
}

pub async fn payback(State(state): State<AppState>) -> ApiResult {
    let (Some(tariff), Some(first)) = (&state.tariff, state.purchases.first()) else {
        return Ok(Json(json!({ "configured": false })));
    };
    let today = today(state.tz);

    // From the first purchase until today (but not unreasonably far back).
    let start = first.date.max(today - Days::new(MAX_PAYBACK_DAYS));
    let dates: Vec<NaiveDate> = start.iter_days().take_while(|d| *d <= today).collect();
    let days: Vec<DaySavings> = billed_days(&state, tariff, &dates)
        .await?
        .into_iter()
        .map(|d| (d.date, d.cost.savings_cents / 100.0))
        .collect();

    let total_cost: f64 = state.purchases.iter().map(|p| p.cost_dollars).sum();
    let saved: f64 = days.iter().map(|d| d.1).sum();
    let net = saved - total_cost;
    let rate = rate_per_day(&days);
    let projected: Value = match projected_payback(net, rate, state.price_rise_percent, today) {
        Projection::PaidBack => Value::Null,
        Projection::Never => json!("never"),
        Projection::On(date) => json!(date.format("%Y-%m").to_string()),
    };
    let purchases: Vec<Value> = state
        .purchases
        .iter()
        .map(|p| json!({ "name": p.name, "date": p.date.to_string(), "cost": round2(p.cost_dollars) }))
        .collect();

    Ok(Json(json!({
        "configured": true,
        "total_cost": round2(total_cost),
        "saved": round2(saved),
        "net": round2(net),
        "recovered_pct": if total_cost > 0.0 { round2(saved / total_cost * 100.0) } else { 0.0 },
        "projected_payback": projected,
        "rate_per_day": round2(rate),
        "price_rise_percent": state.price_rise_percent,
        "purchases": purchases,
        "series": monthly_series(&days, &state.purchases, start, today),
        "by_year": by_year(&days),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn purchase(on: &str, cost_dollars: f64) -> Purchase {
        Purchase {
            name: "x".to_string(),
            date: date(on),
            cost_dollars,
        }
    }

    #[test]
    fn month_ends_include_leap_days_and_december() {
        assert_eq!(month_end(date("2024-02-10")), date("2024-02-29"));
        assert_eq!(month_end(date("2023-02-10")), date("2023-02-28"));
        assert_eq!(month_end(date("2025-12-31")), date("2025-12-31"));
    }

    #[test]
    fn series_accumulates_savings_and_subtracts_purchases_when_due() {
        let days = [
            (date("2025-01-30"), 10.0),
            (date("2025-01-31"), 5.0),
            (date("2025-03-02"), 20.0),
        ];
        let purchases = [purchase("2025-01-15", 100.0), purchase("2025-03-01", 50.0)];
        let series = monthly_series(&days, &purchases, date("2025-01-15"), date("2025-03-10"));
        let got: Vec<(&str, f64, f64)> = series
            .iter()
            .map(|p| (p.month.as_str(), p.saved, p.net))
            .collect();
        assert_eq!(
            got,
            vec![
                ("2025-01", 15.0, -85.0),
                // February has no data: the totals carry over.
                ("2025-02", 15.0, -85.0),
                // The second purchase falls inside March.
                ("2025-03", 35.0, -115.0),
            ]
        );
    }

    #[test]
    fn series_is_empty_when_the_start_is_after_the_end() {
        let series = monthly_series(&[], &[], date("2026-01-01"), date("2025-12-31"));
        assert!(series.is_empty());
    }

    #[test]
    fn rate_uses_the_last_365_days_with_data() {
        assert_eq!(rate_per_day(&[]), 0.0);
        let one = [(date("2025-01-01"), 3.0)];
        assert_eq!(rate_per_day(&one), 3.0);
        // 100 old days at 10, then 365 recent days at 2: only the recent count.
        let first = date("2020-01-01");
        let days: Vec<DaySavings> = (0..465)
            .map(|i| (first + Days::new(i), if i < 100 { 10.0 } else { 2.0 }))
            .collect();
        assert_eq!(rate_per_day(&days), 2.0);
    }

    #[test]
    fn projection_covers_paid_back_never_and_a_date() {
        let today = date("2026-10-21");
        assert_eq!(
            projected_payback(0.0, 5.0, 0.0, today),
            Projection::PaidBack
        );
        assert_eq!(
            projected_payback(50.0, -1.0, 3.0, today),
            Projection::PaidBack
        );
        assert_eq!(
            projected_payback(-100.0, 0.0, 3.0, today),
            Projection::Never
        );
        assert_eq!(
            projected_payback(-100.0, -2.0, 3.0, today),
            Projection::Never
        );
        // 100 / 3 = 33.3, so 34 whole days.
        assert_eq!(
            projected_payback(-100.0, 3.0, 0.0, today),
            Projection::On(date("2026-11-24"))
        );
        // Too far away to mean anything.
        assert_eq!(projected_payback(-1e9, 0.01, 0.0, today), Projection::Never);
    }

    #[test]
    fn price_rise_brings_the_payback_forward() {
        let today = date("2026-10-21");
        // 10 years of savings at 10 a day is 36525, so the linear answer is
        // 10 years out; 3% a year growth gets there sooner.
        let linear = projected_payback(-36_525.0, 10.0, 0.0, today);
        let rising = projected_payback(-36_525.0, 10.0, 3.0, today);
        let (Projection::On(linear), Projection::On(rising)) = (linear, rising) else {
            panic!("expected dates");
        };
        assert_eq!(linear, today + Days::new(3653));
        // ln(1 + 36525 * ln(1.03) / 3652.5) / ln(1.03) is about 8.8 years.
        assert!(
            rising < date("2035-08-31") && rising > date("2035-06-01"),
            "{rising}"
        );
        // Still capped when far away, and still never without savings.
        assert_eq!(
            projected_payback(-1e12, 0.01, 3.0, today),
            Projection::Never
        );
        assert_eq!(
            projected_payback(-100.0, 0.0, 3.0, today),
            Projection::Never
        );
    }

    #[test]
    fn years_are_marked_partial_when_data_is_missing() {
        let mut days: Vec<DaySavings> = (0..367)
            .map(|i| (date("2024-01-01") + Days::new(i), 1.0))
            .collect();
        // 2024 is complete (366 days, from 2024-01-01); 2025 has one day.
        assert_eq!(
            by_year(&days),
            vec![
                YearTotal {
                    year: 2024,
                    saved: 366.0,
                    days: 366,
                    partial: false
                },
                YearTotal {
                    year: 2025,
                    saved: 1.0,
                    days: 1,
                    partial: true
                },
            ]
        );
        // Without the leap day 2024 would be partial.
        days.retain(|d| d.0 != date("2024-02-29"));
        assert!(by_year(&days)[0].partial);
        assert!(by_year(&[]).is_empty());
    }

    #[test]
    fn purchases_are_sorted_by_date() {
        let list = |name: &str, date: &str| config::Purchase {
            name: name.to_string(),
            date: date.to_string(),
            cost_dollars: 1.0,
        };
        let sorted =
            purchases_from_config(&[list("late", "2025-10-01"), list("early", "2024-01-01")])
                .unwrap();
        assert_eq!(sorted[0].name, "early");
        assert!(purchases_from_config(&[list("bad", "nope")]).is_err());
    }
}
