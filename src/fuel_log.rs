//! Fuel economy and petrol spend for the cars TeslaMate does not track (the
//! `manual_car` and `fuel_fill` tables). Everything here is a pure function of
//! the fills and the petrol price history, so it is easy to test.
//!
//! Economy uses the usual full-to-full method: a fill only gives a reading
//! when the tank was filled to full and the odometer was noted, and it is
//! measured against the last such fill. All the petrol put in between (partial
//! fills included) counts towards the distance between the two.

// Nothing calls this until the API that serves it (the next change).
#![allow(dead_code)]

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use serde::Serialize;

use crate::db::FuelFill;
use crate::ev_cost::PetrolHistory;

/// One full-to-full stretch of driving.
struct Segment {
    km: f64,
    litres: f64,
    dollars: f64,
}

/// Totals over all of a car's fills.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Totals {
    pub litres: f64,
    pub cost_dollars: f64,
    pub fills: usize,
    /// First and last fill day, YYYY-MM-DD; `None` when there are no fills.
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    /// Last odometer reading minus the first; `None` with fewer than two
    /// readings (or when they do not increase).
    pub km_tracked: Option<f64>,
    /// What the petrol cost per litre on average, in cents.
    pub avg_cents_per_litre: Option<f64>,
}

/// One calendar month of fills.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthSummary {
    /// YYYY-MM.
    pub month: String,
    pub litres: f64,
    pub cost_dollars: f64,
    pub fills: usize,
    /// Distance driven since the previous odometer reading, summed over the
    /// month's fills (so a fill counts the km that led up to it).
    pub km: f64,
}

/// What is worked out for one fill (the fill itself is not repeated).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FillDetail {
    pub id: i64,
    /// What this fill cost per litre, in cents.
    pub cents_per_litre: f64,
    /// The fuel economy of the full-to-full stretch that ends at this fill.
    pub l_per_100km: Option<f64>,
    /// Distance since the previous fill that has an odometer reading.
    pub km_since_last: Option<f64>,
    /// The average petrol price in the fill's month, in cents per litre.
    pub avg_cents_per_litre: Option<f64>,
}

/// Everything worked out from one car's fills.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FuelSummary {
    pub totals: Totals,
    /// Litres per 100 km over all measured stretches; `None` when there is none.
    pub l_per_100km: Option<f64>,
    /// Petrol dollars per km over the same stretches.
    pub cost_per_km: Option<f64>,
    /// Oldest month first.
    pub months: Vec<MonthSummary>,
    /// Oldest fill first.
    pub fills: Vec<FillDetail>,
}

/// Works out the summary. `fills` may be in any order; `petrol` gives the
/// average price each month is compared with.
pub fn summarise(fills: &[FuelFill], petrol: &PetrolHistory) -> FuelSummary {
    let mut sorted: Vec<&FuelFill> = fills.iter().collect();
    sorted.sort_by(|a, b| {
        let odometer = |f: &FuelFill| f.odometer_km.unwrap_or(f64::NEG_INFINITY);
        a.date
            .cmp(&b.date)
            .then(odometer(a).total_cmp(&odometer(b)))
            .then(a.id.cmp(&b.id))
    });

    let mut details = Vec::with_capacity(sorted.len());
    let mut segments: Vec<Segment> = Vec::new();
    let mut months: BTreeMap<String, MonthSummary> = BTreeMap::new();
    let mut month_prices: BTreeMap<String, Option<f64>> = BTreeMap::new();
    // The last fill with an odometer reading, and the last full one with it
    // (the start of the next stretch, as an index into `sorted`).
    let mut last_odometer: Option<f64> = None;
    let mut anchor: Option<usize> = None;
    let mut odometers: Vec<f64> = Vec::new();

    for (i, fill) in sorted.iter().enumerate() {
        let month = month_of(&fill.date);
        let km_since_last = match (fill.odometer_km, last_odometer) {
            (Some(now), Some(before)) if now > before => Some(now - before),
            _ => None,
        };

        let mut segment_l_per_100km = None;
        if fill.full_tank
            && let Some(odometer) = fill.odometer_km
        {
            if let Some(start) = anchor
                && let Some(from) = sorted[start].odometer_km
                && odometer > from
            {
                let stretch = &sorted[start + 1..=i];
                let segment = Segment {
                    km: odometer - from,
                    litres: stretch.iter().map(|f| f.litres).sum(),
                    dollars: stretch.iter().map(|f| f.cost_dollars).sum(),
                };
                segment_l_per_100km = Some(segment.litres / segment.km * 100.0);
                segments.push(segment);
            }
            anchor = Some(i);
        }
        if let Some(odometer) = fill.odometer_km {
            last_odometer = Some(odometer);
            odometers.push(odometer);
        }

        let entry = months.entry(month.clone()).or_insert_with(|| MonthSummary {
            month: month.clone(),
            litres: 0.0,
            cost_dollars: 0.0,
            fills: 0,
            km: 0.0,
        });
        entry.litres += fill.litres;
        entry.cost_dollars += fill.cost_dollars;
        entry.fills += 1;
        entry.km += km_since_last.unwrap_or(0.0);

        let avg_cents_per_litre = *month_prices
            .entry(month)
            .or_insert_with_key(|month| month_average(petrol, month));
        details.push(FillDetail {
            id: fill.id,
            cents_per_litre: cents_per_litre(fill.cost_dollars, fill.litres),
            l_per_100km: segment_l_per_100km,
            km_since_last,
            avg_cents_per_litre,
        });
    }

    let litres: f64 = sorted.iter().map(|f| f.litres).sum();
    let cost_dollars: f64 = sorted.iter().map(|f| f.cost_dollars).sum();
    let km_tracked = match (odometers.first(), odometers.last()) {
        (Some(first), Some(last)) if odometers.len() >= 2 && last > first => Some(last - first),
        _ => None,
    };
    let segment_km: f64 = segments.iter().map(|s| s.km).sum();
    let segment_litres: f64 = segments.iter().map(|s| s.litres).sum();
    let segment_dollars: f64 = segments.iter().map(|s| s.dollars).sum();

    FuelSummary {
        totals: Totals {
            litres,
            cost_dollars,
            fills: sorted.len(),
            first_date: sorted.first().map(|f| f.date.clone()),
            last_date: sorted.last().map(|f| f.date.clone()),
            km_tracked,
            avg_cents_per_litre: (litres > 0.0).then(|| cents_per_litre(cost_dollars, litres)),
        },
        l_per_100km: (segment_km > 0.0).then(|| segment_litres / segment_km * 100.0),
        cost_per_km: (segment_km > 0.0).then(|| segment_dollars / segment_km),
        months: months.into_values().collect(),
        fills: details,
    }
}

fn cents_per_litre(cost_dollars: f64, litres: f64) -> f64 {
    cost_dollars / litres * 100.0
}

/// "YYYY-MM" of a "YYYY-MM-DD" day.
fn month_of(date: &str) -> String {
    date.get(..7).unwrap_or(date).to_string()
}

/// The mean of the petrol price on each day of a "YYYY-MM" month, over the
/// days that have a price; `None` when none do (or the month is not valid).
fn month_average(petrol: &PetrolHistory, month: &str) -> Option<f64> {
    let first = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").ok()?;
    let prices: Vec<f64> = first
        .iter_days()
        .take_while(|day| day.month() == first.month())
        .filter_map(|day| petrol.on(&day.format("%Y-%m-%d").to_string()))
        .collect();
    (!prices.is_empty()).then(|| prices.iter().sum::<f64>() / prices.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(
        id: i64,
        date: &str,
        litres: f64,
        dollars: f64,
        odo: Option<f64>,
        full: bool,
    ) -> FuelFill {
        FuelFill {
            id,
            car_id: 1,
            date: date.to_string(),
            litres,
            cost_dollars: dollars,
            odometer_km: odo,
            full_tank: full,
            note: String::new(),
        }
    }

    fn no_prices() -> PetrolHistory {
        PetrolHistory::default()
    }

    fn close(a: Option<f64>, b: f64) {
        let a = a.expect("a value");
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }

    #[test]
    fn no_fills_gives_an_empty_summary() {
        let s = summarise(&[], &no_prices());
        assert_eq!(s.totals.fills, 0);
        assert_eq!(s.totals.first_date, None);
        assert_eq!(s.totals.avg_cents_per_litre, None);
        assert_eq!((s.l_per_100km, s.cost_per_km), (None, None));
        assert!(s.months.is_empty() && s.fills.is_empty());
    }

    #[test]
    fn the_first_full_fill_only_starts_the_chain() {
        let fills = [fill(1, "2026-10-01", 40.0, 80.0, Some(1000.0), true)];
        let s = summarise(&fills, &no_prices());
        assert_eq!((s.l_per_100km, s.cost_per_km), (None, None));
        assert_eq!(s.fills[0].l_per_100km, None);
        assert_eq!(s.totals.km_tracked, None);
        close(Some(s.fills[0].cents_per_litre), 200.0);
    }

    #[test]
    fn economy_is_measured_full_to_full_with_partial_fills_in_between() {
        let fills = [
            fill(1, "2026-10-01", 40.0, 80.0, Some(1000.0), true),
            // A top-up with no odometer reading still counts towards the stretch.
            fill(2, "2026-10-05", 10.0, 21.0, None, false),
            fill(3, "2026-10-09", 20.0, 42.0, Some(1500.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        // 30 L over 500 km.
        close(s.l_per_100km, 6.0);
        close(s.cost_per_km, 63.0 / 500.0);
        close(s.fills[2].l_per_100km, 6.0);
        assert_eq!(s.fills[1].l_per_100km, None);
        assert_eq!(s.fills[2].km_since_last, Some(500.0));
        assert_eq!(s.totals.km_tracked, Some(500.0));
        close(Some(s.totals.litres), 70.0);
        close(s.totals.avg_cents_per_litre, 143.0 / 70.0 * 100.0);
    }

    #[test]
    fn a_partial_fill_with_an_odometer_gives_km_but_no_economy() {
        let fills = [
            fill(1, "2026-10-01", 40.0, 80.0, Some(1000.0), true),
            fill(2, "2026-10-04", 10.0, 20.0, Some(1200.0), false),
            fill(3, "2026-10-08", 30.0, 60.0, Some(1600.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        assert_eq!(s.fills[1].l_per_100km, None);
        assert_eq!(s.fills[1].km_since_last, Some(200.0));
        // 40 L over the 600 km from the first full fill.
        close(s.fills[2].l_per_100km, 40.0 / 600.0 * 100.0);
        close(s.l_per_100km, 40.0 / 600.0 * 100.0);
    }

    #[test]
    fn stretches_are_summed_not_averaged() {
        let fills = [
            fill(1, "2026-10-01", 40.0, 80.0, Some(0.0), true),
            fill(2, "2026-10-10", 10.0, 20.0, Some(100.0), true),
            fill(3, "2026-10-20", 30.0, 60.0, Some(500.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        // (10 + 30) L over 500 km, not the mean of 10 and 7.5.
        close(s.l_per_100km, 8.0);
        close(s.fills[1].l_per_100km, 10.0);
        close(s.fills[2].l_per_100km, 7.5);
    }

    #[test]
    fn fills_in_any_order_are_sorted_by_date_odometer_then_id() {
        let fills = [
            fill(9, "2026-10-09", 20.0, 42.0, Some(1500.0), true),
            fill(1, "2026-10-01", 40.0, 80.0, Some(1000.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        let ids: Vec<i64> = s.fills.iter().map(|f| f.id).collect();
        assert_eq!(ids, [1, 9]);
        assert_eq!(s.totals.first_date.as_deref(), Some("2026-10-01"));
        assert_eq!(s.totals.last_date.as_deref(), Some("2026-10-09"));

        // Same day: the lower odometer reading, then the lower id.
        let same_day = [
            fill(5, "2026-10-01", 1.0, 2.0, Some(120.0), false),
            fill(4, "2026-10-01", 1.0, 2.0, Some(100.0), false),
            fill(3, "2026-10-01", 1.0, 2.0, Some(100.0), false),
        ];
        let ids: Vec<i64> = summarise(&same_day, &no_prices())
            .fills
            .iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, [3, 4, 5]);
    }

    #[test]
    fn an_odometer_that_does_not_increase_gives_no_segment() {
        let fills = [
            fill(1, "2026-10-01", 40.0, 80.0, Some(1000.0), true),
            fill(2, "2026-10-05", 40.0, 80.0, Some(1000.0), true),
            fill(3, "2026-10-09", 40.0, 80.0, Some(1400.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        assert_eq!(s.fills[1].l_per_100km, None);
        assert_eq!(s.fills[1].km_since_last, None);
        // The stretch restarts from the second reading.
        close(s.l_per_100km, 10.0);
    }

    #[test]
    fn months_add_up_and_attribute_km_to_the_later_fill() {
        let fills = [
            fill(1, "2026-09-28", 40.0, 80.0, Some(1000.0), true),
            fill(2, "2026-10-03", 30.0, 60.0, Some(1400.0), true),
            fill(3, "2026-10-20", 20.0, 44.0, Some(1900.0), true),
        ];
        let s = summarise(&fills, &no_prices());
        assert_eq!(s.months.len(), 2);
        assert_eq!(s.months[0].month, "2026-09");
        assert_eq!((s.months[0].fills, s.months[0].km), (1, 0.0));
        let october = &s.months[1];
        assert_eq!(october.month, "2026-10");
        assert_eq!(october.fills, 2);
        close(Some(october.litres), 50.0);
        close(Some(october.cost_dollars), 104.0);
        close(Some(october.km), 900.0);
    }

    #[test]
    fn the_month_average_comes_from_the_petrol_price_history() {
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        // 200 c/L for the first half of October (15 days), 100 c/L after (16).
        let petrol = PetrolHistory::new(None, vec![(day(1), 200.0), (day(16), 100.0)]);
        let fills = [
            fill(1, "2026-10-02", 10.0, 22.0, None, true),
            fill(2, "2026-09-02", 10.0, 22.0, None, true),
        ];
        let s = summarise(&fills, &petrol);
        // September (first in time) has no price at all.
        assert_eq!(s.fills[0].avg_cents_per_litre, None);
        close(
            s.fills[1].avg_cents_per_litre,
            (15.0 * 200.0 + 16.0 * 100.0) / 31.0,
        );
        close(Some(s.fills[1].cents_per_litre), 220.0);

        // A base price covers the months before the first change.
        let petrol = PetrolHistory::new(Some(190.0), vec![]);
        let s = summarise(&fills, &petrol);
        close(s.fills[0].avg_cents_per_litre, 190.0);
    }
}
