//! `/api/payback`: how much of what the system cost has been saved so far,
//! and when it should be paid back.
//!
//! The savings come from [`billed_days`]: our cached estimate, with GloBird's
//! own figures put in for the days it has billed. The
//! maths below is pure and works on plain `(date, dollars)` pairs, so it is
//! tested with plain numbers.
//!
//! A later purchase with a `battery_kwh` also gets a rough estimate of what
//! its battery added, from the battery's daily discharge (as `/api/daily`
//! reports it) priced at that day's peak import rate. It splits the savings
//! into a part per purchase, and shows the net position without the latest
//! battery.

use std::collections::BTreeMap;

use anyhow::Result;
use axum::Json;
use axum::extract::State;
use chrono::{Datelike, Days, Months, NaiveDate};
use serde::Serialize;
use serde_json::{Value, json};

use super::AppState;
use super::api::{ApiResult, battery_discharge_by_date, today};
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
/// The share of a battery's capacity assumed to be used each day. The
/// discharge up to this much from the batteries already owned is not credited
/// to a new one.
const BATTERY_USABLE: f64 = 0.95;

/// One thing bought for the system, with its date already parsed.
#[derive(Debug, Clone)]
pub struct Purchase {
    pub name: String,
    pub date: NaiveDate,
    pub cost_dollars: f64,
    /// Capacity of the battery it added, in kWh, if any.
    pub battery_kwh: Option<f64>,
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
                battery_kwh: p.battery_kwh,
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

/// The last [`RATE_DAYS`] days that have data (all of them if there are
/// fewer). `days` must be oldest first.
fn recent(days: &[DaySavings]) -> &[DaySavings] {
    &days[days.len().saturating_sub(RATE_DAYS)..]
}

/// Savings over the days [`rate_per_day`] averages.
fn last_12_months_saved(days: &[DaySavings]) -> f64 {
    recent(days).iter().map(|d| d.1).sum()
}

/// Mean savings per day over the last [`RATE_DAYS`] days that have data (all
/// of them if there are fewer). 0 with no data. `days` must be oldest first.
fn rate_per_day(days: &[DaySavings]) -> f64 {
    let recent = recent(days);
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

/// Dollars a battery adds on one day: the discharge beyond what the batteries
/// owned before it could supply (`BATTERY_USABLE` of their capacity), priced
/// at the day's peak import rate in cents. Never negative.
fn added_savings(discharge_kwh: f64, owned_kwh: f64, peak_cents: f64) -> f64 {
    (discharge_kwh - BATTERY_USABLE * owned_kwh).max(0.0) * peak_cents / 100.0
}

/// What a battery added on each day from `from` that has a discharge figure,
/// in dollars. `peak_cents` gives a day's peak import rate. Days without a
/// figure are left out, so they add nothing.
fn estimated_added(
    discharge: &BTreeMap<NaiveDate, f64>,
    from: NaiveDate,
    owned_kwh: f64,
    peak_cents: impl Fn(NaiveDate) -> f64,
) -> BTreeMap<NaiveDate, f64> {
    discharge
        .range(from..)
        .map(|(&date, &kwh)| (date, added_savings(kwh, owned_kwh, peak_cents(date))))
        .collect()
}

/// `days` with each map's added savings taken off its days. Same days, same order.
fn minus_added(days: &[DaySavings], added: &[&BTreeMap<NaiveDate, f64>]) -> Vec<DaySavings> {
    days.iter()
        .map(|&(date, saved)| {
            let taken: f64 = added.iter().filter_map(|a| a.get(&date)).sum();
            (date, saved - taken)
        })
        .collect()
}

/// What `added` gives on the days of `days` from `from` (0 where it has none),
/// so a rate over it covers every day since the purchase.
fn added_on_days(
    days: &[DaySavings],
    added: &BTreeMap<NaiveDate, f64>,
    from: NaiveDate,
) -> Vec<DaySavings> {
    days.iter()
        .filter(|d| d.0 >= from)
        .map(|&(date, _)| (date, added.get(&date).copied().unwrap_or(0.0)))
        .collect()
}

/// The kWh of battery owned before purchase `index`: the `battery_kwh` of
/// every earlier purchase. `purchases` must be oldest first.
fn battery_owned_before(purchases: &[Purchase], index: usize) -> f64 {
    purchases[..index]
        .iter()
        .filter_map(|p| p.battery_kwh)
        .sum()
}

/// One purchase with its share of the savings.
#[derive(Debug, PartialEq)]
struct Part {
    /// `None` for a later purchase with no `battery_kwh`: its savings stay
    /// with the first purchase.
    saved: Option<f64>,
    /// `saved` is the battery estimate, not a measured total.
    estimated: bool,
    /// When this part alone pays for itself, at its own recent rate.
    projection: Option<Projection>,
}

/// A part per purchase, oldest first. `added` has one entry per purchase: the
/// estimated daily savings of a later purchase with a `battery_kwh`, `None`
/// for the rest. The first purchase gets the savings less every estimate, so
/// the parts add up to the total.
fn build_parts(
    purchases: &[Purchase],
    added: &[Option<BTreeMap<NaiveDate, f64>>],
    days: &[DaySavings],
    price_rise_percent: f64,
    today: NaiveDate,
) -> Vec<Part> {
    let estimates: Vec<&BTreeMap<NaiveDate, f64>> = added.iter().flatten().collect();
    let first_days = minus_added(days, &estimates);
    let estimated_total: f64 = estimates.iter().flat_map(|a| a.values()).sum();
    let total: f64 = days.iter().map(|d| d.1).sum();
    purchases
        .iter()
        .zip(added)
        .enumerate()
        .map(|(i, (purchase, added))| {
            let (saved, rate, estimated) = match (i, added) {
                (_, Some(added)) => {
                    let own = added_on_days(days, added, purchase.date);
                    (Some(added.values().sum()), rate_per_day(&own), true)
                }
                (0, None) => (
                    Some(total - estimated_total),
                    rate_per_day(&first_days),
                    false,
                ),
                _ => (None, 0.0, false),
            };
            let projection = saved.map(|saved| {
                projected_payback(
                    saved - purchase.cost_dollars,
                    rate,
                    price_rise_percent,
                    today,
                )
            });
            Part {
                saved,
                estimated,
                projection,
            }
        })
        .collect()
}

/// A date on the ownership timeline.
#[derive(Debug, PartialEq, Serialize)]
struct TimelineEntry {
    /// "YYYY-MM-DD".
    date: String,
    /// "purchase" or "plan".
    kind: &'static str,
    label: String,
    /// Dollars, for a purchase.
    cost: Option<f64>,
}

/// Every purchase and every named plan start, oldest first (a purchase before
/// a plan on the same day).
fn build_timeline(purchases: &[Purchase], plans: &[(NaiveDate, String)]) -> Vec<TimelineEntry> {
    let mut entries: Vec<TimelineEntry> = purchases
        .iter()
        .map(|p| TimelineEntry {
            date: p.date.to_string(),
            kind: "purchase",
            label: p.name.clone(),
            cost: Some(round2(p.cost_dollars)),
        })
        .chain(plans.iter().map(|(date, name)| TimelineEntry {
            date: date.to_string(),
            kind: "plan",
            label: name.clone(),
            cost: None,
        }))
        .collect();
    // Stable, so a purchase stays ahead of a plan on the same day.
    entries.sort_by(|a, b| a.date.cmp(&b.date));
    entries
}

/// The projection as the API reports it: `null` once paid back, "never", or "YYYY-MM".
fn projection_json(projection: &Projection) -> Value {
    match projection {
        Projection::PaidBack => Value::Null,
        Projection::Never => json!("never"),
        Projection::On(date) => json!(date.format("%Y-%m").to_string()),
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
    let data = state.admin.get();
    let (Some(tariff), Some(first)) = (&data.tariff, data.purchases.first()) else {
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

    let total_cost: f64 = data.purchases.iter().map(|p| p.cost_dollars).sum();
    let saved: f64 = days.iter().map(|d| d.1).sum();
    let net = saved - total_cost;
    let rate = rate_per_day(&days);
    let projected = projection_json(&projected_payback(
        net,
        rate,
        data.price_rise_percent,
        today,
    ));
    let purchases: Vec<Value> = data
        .purchases
        .iter()
        .map(|p| json!({ "name": p.name, "date": p.date.to_string(), "cost": round2(p.cost_dollars) }))
        .collect();

    // What each later battery added, from the days it has a discharge figure for.
    let battery_from = data
        .purchases
        .iter()
        .skip(1)
        .filter(|p| p.battery_kwh.is_some())
        .map(|p| p.date.max(start))
        .min();
    let discharge = match battery_from {
        Some(from) => {
            let dates: Vec<NaiveDate> = from.iter_days().take_while(|d| *d <= today).collect();
            battery_discharge_by_date(&state, &dates).await?
        }
        None => BTreeMap::new(),
    };
    let added: Vec<Option<BTreeMap<NaiveDate, f64>>> = data
        .purchases
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == 0 || p.battery_kwh.is_none() {
                return None;
            }
            let owned = battery_owned_before(&data.purchases, i);
            Some(estimated_added(&discharge, p.date, owned, |date| {
                tariff.for_date(date).peak_import_cents()
            }))
        })
        .collect();
    let parts = build_parts(
        &data.purchases,
        &added,
        &days,
        data.price_rise_percent,
        today,
    );
    let parts: Vec<Value> = data
        .purchases
        .iter()
        .zip(&parts)
        .map(|(p, part)| {
            let net = part.saved.map(|saved| saved - p.cost_dollars);
            json!({
                "name": p.name,
                "date": p.date.to_string(),
                "cost": round2(p.cost_dollars),
                "saved": part.saved.map(round2),
                "estimated": part.estimated,
                "recovered_pct": part.saved.map(|saved| if p.cost_dollars > 0.0 {
                    round2(saved / p.cost_dollars * 100.0)
                } else {
                    0.0
                }),
                "net": net.map(round2),
                "projected_payback": part.projection.as_ref().map_or(Value::Null, projection_json),
            })
        })
        .collect();

    // Without the latest battery: only when its savings were estimated.
    let without = match (data.purchases.split_last(), added.last()) {
        (Some((last, earlier)), Some(Some(added))) if !earlier.is_empty() => {
            let days = minus_added(&days, &[added]);
            let saved: f64 = days.iter().map(|d| d.1).sum();
            let cost: f64 = earlier.iter().map(|p| p.cost_dollars).sum();
            let rate = rate_per_day(&days);
            let projection = projected_payback(saved - cost, rate, data.price_rise_percent, today);
            json!({
                "name": last.name,
                "series": monthly_series(&days, earlier, start, today),
                "rate_per_day": round2(rate),
                "projected_payback": projection_json(&projection),
            })
        }
        _ => Value::Null,
    };

    let first_day = first.date;
    let plans: Vec<(NaiveDate, String)> = tariff
        .plans()
        .into_iter()
        .filter_map(|(date, name)| {
            let name = name?.trim();
            (!name.is_empty()).then(|| (date.unwrap_or(first_day), name.to_string()))
        })
        .collect();

    Ok(Json(json!({
        "configured": true,
        "total_cost": round2(total_cost),
        "saved": round2(saved),
        "net": round2(net),
        "recovered_pct": if total_cost > 0.0 { round2(saved / total_cost * 100.0) } else { 0.0 },
        "projected_payback": projected,
        "rate_per_day": round2(rate),
        "last_12_months_saved": round2(last_12_months_saved(&days)),
        "price_rise_percent": data.price_rise_percent,
        "purchases": purchases,
        "parts": parts,
        "without": without,
        "timeline": build_timeline(&data.purchases, &plans),
        "series": monthly_series(&days, &data.purchases, start, today),
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
            battery_kwh: None,
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

    fn battery(on: &str, cost_dollars: f64, kwh: f64) -> Purchase {
        Purchase {
            battery_kwh: Some(kwh),
            ..purchase(on, cost_dollars)
        }
    }

    #[test]
    fn a_battery_adds_only_the_discharge_beyond_what_was_owned() {
        // 10 kWh owned supplies 9.5; 12 kWh discharged leaves 2.5 at 50 c.
        assert!((added_savings(12.0, 10.0, 50.0) - 1.25).abs() < 1e-9);
        // Less than the old battery could supply: nothing, never negative.
        assert_eq!(added_savings(9.0, 10.0, 50.0), 0.0);
        // With no battery before, all of it counts.
        assert!((added_savings(4.0, 0.0, 25.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_estimate_starts_on_the_purchase_day_and_skips_days_without_data() {
        let discharge: BTreeMap<NaiveDate, f64> = [
            (date("2025-09-30"), 20.0),
            (date("2025-10-01"), 20.0),
            // 2025-10-02 has no figure.
            (date("2025-10-03"), 10.0),
        ]
        .into();
        // Peak 40 c before the 3rd, 80 c from then on.
        let peak = |d: NaiveDate| if d < date("2025-10-03") { 40.0 } else { 80.0 };
        let added = estimated_added(&discharge, date("2025-10-01"), 10.0, peak);
        let got: Vec<(NaiveDate, f64)> = added.into_iter().collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, date("2025-10-01"));
        // (20 - 9.5) * 0.40 and (10 - 9.5) * 0.80.
        assert!((got[0].1 - 4.2).abs() < 1e-9);
        assert_eq!(got[1].0, date("2025-10-03"));
        assert!((got[1].1 - 0.4).abs() < 1e-9);
    }

    #[test]
    fn owned_battery_adds_up_the_earlier_purchases() {
        let list = [
            battery("2023-02-01", 1.0, 13.0),
            purchase("2024-01-01", 1.0),
            battery("2025-10-01", 1.0, 12.8),
        ];
        assert_eq!(battery_owned_before(&list, 0), 0.0);
        assert_eq!(battery_owned_before(&list, 1), 13.0);
        assert_eq!(battery_owned_before(&list, 2), 13.0);
    }

    #[test]
    fn minusing_the_added_savings_keeps_the_days() {
        let days = [
            (date("2025-01-01"), 10.0),
            (date("2025-01-02"), 10.0),
            (date("2025-01-03"), 10.0),
        ];
        let one: BTreeMap<NaiveDate, f64> = [(date("2025-01-02"), 3.0)].into();
        let two: BTreeMap<NaiveDate, f64> =
            [(date("2025-01-02"), 1.0), (date("2025-01-03"), 2.0)].into();
        assert_eq!(
            minus_added(&days, &[&one, &two]),
            vec![
                (date("2025-01-01"), 10.0),
                (date("2025-01-02"), 6.0),
                (date("2025-01-03"), 8.0),
            ]
        );
        // The part's own days run from the purchase, with 0 where nothing was added.
        assert_eq!(
            added_on_days(&days, &one, date("2025-01-02")),
            vec![(date("2025-01-02"), 3.0), (date("2025-01-03"), 0.0)]
        );
    }

    #[test]
    fn last_12_months_use_the_rate_window() {
        let first = date("2020-01-01");
        let days: Vec<DaySavings> = (0..400)
            .map(|i| (first + Days::new(i), if i < 35 { 100.0 } else { 1.0 }))
            .collect();
        assert_eq!(last_12_months_saved(&days), 365.0);
        assert_eq!(last_12_months_saved(&[]), 0.0);
    }

    #[test]
    fn parts_add_up_to_the_total() {
        let today = date("2026-10-21");
        let purchases = [
            battery("2025-01-01", 1000.0, 10.0),
            purchase("2025-02-01", 50.0),
            battery("2025-03-01", 100.0, 5.0),
        ];
        let days: Vec<DaySavings> = (0..100)
            .map(|i| (date("2025-01-01") + Days::new(i), 10.0))
            .collect();
        // The third purchase added 4 a day from 2025-03-01 (day 59) to day 99.
        let added: BTreeMap<NaiveDate, f64> = (59..100)
            .map(|i| (date("2025-01-01") + Days::new(i), 4.0))
            .collect();
        let parts = build_parts(&purchases, &[None, None, Some(added)], &days, 0.0, today);
        assert_eq!(parts.len(), 3);
        // 1000 total less 41 days * 4.
        assert_eq!(parts[0].saved, Some(836.0));
        assert!(!parts[0].estimated);
        // Not estimated, so nothing of its own.
        assert_eq!(parts[1].saved, None);
        assert_eq!(parts[1].projection, None);
        assert_eq!(parts[2].saved, Some(164.0));
        assert!(parts[2].estimated);
        // The first part still needs 164 at its own rate: (59 * 10 + 41 * 6) / 100 = 8.36.
        let Some(Projection::On(first)) = parts[0].projection else {
            panic!("expected a date");
        };
        assert_eq!(first, today + Days::new(20));
        // 164 saved against 100: already paid back.
        assert_eq!(parts[2].projection, Some(Projection::PaidBack));
    }

    #[test]
    fn a_part_with_no_savings_never_pays_back() {
        let today = date("2026-10-21");
        let purchases = [
            purchase("2025-01-01", 100.0),
            battery("2025-02-01", 50.0, 5.0),
        ];
        let days = [(date("2025-01-01"), 0.0), (date("2025-02-01"), 0.0)];
        let parts = build_parts(
            &purchases,
            &[None, Some(BTreeMap::new())],
            &days,
            3.0,
            today,
        );
        assert_eq!(parts[0].projection, Some(Projection::Never));
        assert_eq!(parts[1].saved, Some(0.0));
        assert_eq!(parts[1].projection, Some(Projection::Never));
    }

    #[test]
    fn timeline_is_sorted_with_purchases_and_plans() {
        let purchases = [
            purchase("2023-02-01", 19_490.0),
            battery("2025-10-01", 7_690.0, 12.8),
        ];
        let plans = [
            (date("2023-02-01"), "First plan".to_string()),
            (date("2025-07-01"), "Current plan".to_string()),
        ];
        let got: Vec<(String, &str, String, Option<f64>)> = build_timeline(&purchases, &plans)
            .into_iter()
            .map(|e| (e.date, e.kind, e.label, e.cost))
            .collect();
        let expected =
            |date: &str, kind, label: &str, cost| (date.to_string(), kind, label.to_string(), cost);
        assert_eq!(
            got,
            vec![
                expected("2023-02-01", "purchase", "x", Some(19_490.0)),
                expected("2023-02-01", "plan", "First plan", None),
                expected("2025-07-01", "plan", "Current plan", None),
                expected("2025-10-01", "purchase", "x", Some(7_690.0)),
            ]
        );
    }

    #[test]
    fn projection_json_matches_the_top_level_format() {
        assert_eq!(projection_json(&Projection::PaidBack), Value::Null);
        assert_eq!(projection_json(&Projection::Never), json!("never"));
        assert_eq!(
            projection_json(&Projection::On(date("2027-03-09"))),
            json!("2027-03")
        );
    }

    #[test]
    fn purchases_are_sorted_by_date() {
        let list = |name: &str, date: &str| config::Purchase {
            name: name.to_string(),
            date: date.to_string(),
            cost_dollars: 1.0,
            battery_kwh: None,
        };
        let sorted =
            purchases_from_config(&[list("late", "2025-10-01"), list("early", "2024-01-01")])
                .unwrap();
        assert_eq!(sorted[0].name, "early");
        assert!(purchases_from_config(&[list("bad", "nope")]).is_err());
    }
}
