//! `/api/globird/compare`: what GloBird charged each local day next to what
//! our own estimate (`/api/costs`) says the same day cost.
//!
//! GloBird's lines come from the `globird_*` tables. Money is in dollars,
//! signed the way GloBird signs it: a credit is negative. Our `DayCost` holds
//! feed-in and credit as positive numbers, so they are negated here to match.
//! The building blocks are pure functions, tested with plain numbers.

use std::collections::BTreeMap;
use std::str::FromStr;

use axum::Json;
use axum::extract::{Query, State};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::api::{ApiResult, date_range};
use super::costs::{CostedDay, DayKwh, costed_days, kwh_by_date, round2};
use crate::cost::DayCost;
use crate::db::{self, GlobirdCostRow};
use crate::globird::data::{Bucket, bucket_of, is_supply};

/// Without `from`, the compare covers this many days.
const DEFAULT_COMPARE_DAYS: u64 = 31;
/// The longest range one request may ask for.
const MAX_COMPARE_DAYS: u64 = 400;

/// Cents to dollars, rounded to 2 decimals. Adding 0.0 turns -0.0 into 0.0,
/// so a zero never shows up as `-0.0` in the JSON.
fn money(cents: f64) -> f64 {
    round2(cents / 100.0) + 0.0
}

/// One GloBird charge line, as returned.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct LineOut {
    category: String,
    /// Dollars, negative for a credit.
    amount: f64,
    quantity: f64,
}

/// GloBird's money for a day, or for a whole range.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct GlobirdSums {
    import: f64,
    export: f64,
    supply: f64,
    credit: f64,
    other: f64,
    total: f64,
    import_kwh: Option<f64>,
    export_kwh: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GlobirdDay {
    #[serde(flatten)]
    sums: GlobirdSums,
    lines: Vec<LineOut>,
}

/// Our estimate for a day, signed like GloBird's figures.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct OursSums {
    import: f64,
    export: f64,
    supply: f64,
    credit: f64,
    total: f64,
    import_kwh: f64,
    export_kwh: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct OursDay {
    #[serde(flatten)]
    sums: OursSums,
    /// "inverter" or "cloud", as in `/api/costs`.
    source: &'static str,
}

/// Ours minus GloBird's. A kWh difference is `None` when GloBird has no kWh.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Diff {
    total: f64,
    import_kwh: Option<f64>,
    export_kwh: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct CompareRow {
    date: String,
    /// Whether GloBird has posted more than the daily supply charge.
    complete: bool,
    globird: GlobirdDay,
    ours: Option<OursDay>,
    diff: Option<Diff>,
}

/// GloBird's side of one day: the lines, added up per bucket in whole cents
/// (so the buckets always add up to the total).
fn globird_day(lines: &[GlobirdCostRow], kwh: DayKwh) -> GlobirdDay {
    let mut cents = [0i64; 5]; // import, export, supply, credit, other
    for line in lines {
        let slot = match bucket_of(&line.category) {
            Bucket::Import => 0,
            Bucket::Export => 1,
            Bucket::Supply => 2,
            Bucket::Credit => 3,
            Bucket::Other => 4,
        };
        cents[slot] += line.amount_cents;
    }
    let [import, export, supply, credit, other] = cents.map(|c| money(c as f64));
    GlobirdDay {
        sums: GlobirdSums {
            import,
            export,
            supply,
            credit,
            other,
            total: money(cents.iter().sum::<i64>() as f64),
            import_kwh: kwh.import.map(round2),
            export_kwh: kwh.export.map(round2),
        },
        lines: lines
            .iter()
            .map(|line| LineOut {
                category: line.category.clone(),
                amount: money(line.amount_cents as f64),
                quantity: round2(line.quantity),
            })
            .collect(),
    }
}

/// Our side of one day. Export and credit are negated to match GloBird.
fn ours_day(cost: &DayCost, source: &'static str) -> OursDay {
    OursDay {
        sums: OursSums {
            import: money(cost.import_cents),
            export: money(-cost.export_cents),
            supply: money(cost.supply_cents),
            credit: money(-cost.zero_import_credit_cents),
            total: money(cost.actual_cents),
            import_kwh: round2(cost.import_kwh),
            export_kwh: round2(cost.export_kwh),
        },
        source,
    }
}

/// `a - b` where both are known.
fn minus(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    Some(round2(a? - b?) + 0.0)
}

/// One compare row: GloBird's lines for `date` and, when we have one, our
/// estimate for the same day. `diff` is ours minus GloBird's, and is `None`
/// when `ours` is.
fn compare_row(
    date: NaiveDate,
    lines: &[GlobirdCostRow],
    kwh: DayKwh,
    ours: Option<(&DayCost, &'static str)>,
) -> CompareRow {
    let globird = globird_day(lines, kwh);
    let ours = ours.map(|(cost, source)| ours_day(cost, source));
    let diff = ours.as_ref().map(|ours| Diff {
        total: round2(ours.sums.total - globird.sums.total) + 0.0,
        import_kwh: minus(Some(ours.sums.import_kwh), globird.sums.import_kwh),
        export_kwh: minus(Some(ours.sums.export_kwh), globird.sums.export_kwh),
    });
    CompareRow {
        date: date.to_string(),
        complete: lines.iter().any(|line| !is_supply(&line.category)),
        globird,
        ours,
        diff,
    }
}

/// The range totals, over the days where both sides exist.
#[derive(Debug, Default, Serialize)]
struct Totals {
    days_compared: u32,
    globird: GlobirdSums,
    ours: OursSums,
    diff: Diff,
}

/// Adds up the rows that are complete and have both sides (a day GloBird has
/// only posted the supply charge for would look like a big difference). Sums of the rows' own rounded
/// figures, so the totals match the rows exactly. A day where GloBird has no
/// kWh adds nothing to the kWh sums.
fn totals_of(rows: &[CompareRow]) -> Totals {
    let mut totals = Totals::default();
    let (mut globird_import_kwh, mut globird_export_kwh) = (0.0, 0.0);
    for row in rows {
        let Some(ours) = &row.ours else { continue };
        if !row.complete {
            continue;
        }
        let g = &row.globird.sums;
        totals.days_compared += 1;
        totals.globird.import += g.import;
        totals.globird.export += g.export;
        totals.globird.supply += g.supply;
        totals.globird.credit += g.credit;
        totals.globird.other += g.other;
        totals.globird.total += g.total;
        globird_import_kwh += g.import_kwh.unwrap_or(0.0);
        globird_export_kwh += g.export_kwh.unwrap_or(0.0);
        let o = &ours.sums;
        totals.ours.import += o.import;
        totals.ours.export += o.export;
        totals.ours.supply += o.supply;
        totals.ours.credit += o.credit;
        totals.ours.total += o.total;
        totals.ours.import_kwh += o.import_kwh;
        totals.ours.export_kwh += o.export_kwh;
    }
    let g = &mut totals.globird;
    for value in [
        &mut g.import,
        &mut g.export,
        &mut g.supply,
        &mut g.credit,
        &mut g.other,
        &mut g.total,
    ] {
        *value = round2(*value) + 0.0;
    }
    g.import_kwh = Some(round2(globird_import_kwh));
    g.export_kwh = Some(round2(globird_export_kwh));
    let o = &mut totals.ours;
    for value in [
        &mut o.import,
        &mut o.export,
        &mut o.supply,
        &mut o.credit,
        &mut o.total,
        &mut o.import_kwh,
        &mut o.export_kwh,
    ] {
        *value = round2(*value) + 0.0;
    }
    totals.diff = Diff {
        total: round2(totals.ours.total - totals.globird.total) + 0.0,
        import_kwh: minus(Some(totals.ours.import_kwh), totals.globird.import_kwh),
        export_kwh: minus(Some(totals.ours.export_kwh), totals.globird.export_kwh),
    };
    totals
}

/// One GloBird category added up over the range.
#[derive(Debug, PartialEq, Serialize)]
struct CategoryTotal {
    category: String,
    /// Which comparison column it counts in: import, export, supply, credit or other.
    bucket: &'static str,
    /// Dollars, negative for a credit.
    amount: f64,
    /// kWh (1 per day for ZEROHERO credits, 0 for SUPPLY).
    quantity: f64,
    days: u32,
}

/// Every category in `rows`, added up, in alphabetical order. This shows
/// categories the mapping does not know yet (they land in `other`).
fn category_totals(rows: &[GlobirdCostRow]) -> Vec<CategoryTotal> {
    let mut by_category: BTreeMap<&str, (i64, f64, u32)> = BTreeMap::new();
    for row in rows {
        let entry = by_category.entry(&row.category).or_default();
        entry.0 += row.amount_cents;
        entry.1 += row.quantity;
        entry.2 += 1;
    }
    by_category
        .into_iter()
        .map(|(category, (cents, quantity, days))| CategoryTotal {
            category: category.to_string(),
            bucket: bucket_of(category).as_str(),
            amount: money(cents as f64),
            quantity: round2(quantity),
            days,
        })
        .collect()
}

#[derive(Deserialize)]
pub struct CompareParams {
    from: Option<String>,
    to: Option<String>,
}

pub async fn compare(
    State(state): State<AppState>,
    Query(params): Query<CompareParams>,
) -> ApiResult {
    let (from, count) = date_range(
        params.from.as_deref(),
        params.to.as_deref(),
        state.tz,
        MAX_COMPARE_DAYS,
        DEFAULT_COMPARE_DAYS,
    )?;
    let to = from + chrono::Days::new(count - 1);
    let (from_text, to_text) = (from.to_string(), to.to_string());

    let cost_rows = db::globird_cost_in_range(&state.pool, &from_text, &to_text).await?;
    let mut kwh = kwh_by_date(db::globird_kwh_in_range(&state.pool, &from_text, &to_text).await?);

    // GloBird's days, oldest first (the query is ordered by date).
    let mut lines_by_date: BTreeMap<NaiveDate, Vec<GlobirdCostRow>> = BTreeMap::new();
    for row in &cost_rows {
        if let Ok(date) = NaiveDate::from_str(&row.date) {
            lines_by_date.entry(date).or_default().push(row.clone());
        }
    }

    // Our estimate, only for the span GloBird has days in. `costed_days`
    // leaves out days it has no data for, and without a tariff there is none.
    let mut ours: BTreeMap<NaiveDate, CostedDay> = BTreeMap::new();
    if let (Some(tariff), Some(first), Some(last)) = (
        &state.tariff,
        lines_by_date.keys().next(),
        lines_by_date.keys().next_back(),
    ) {
        let dates: Vec<NaiveDate> = first.iter_days().take_while(|d| d <= last).collect();
        for day in costed_days(&state, tariff, &dates).await? {
            ours.insert(day.date, day);
        }
    }

    let rows: Vec<CompareRow> = lines_by_date
        .iter()
        .map(|(date, lines)| {
            let day_kwh = kwh.remove(&date.to_string()).unwrap_or_default();
            let estimate = ours.get(date).map(|d| (&d.cost, d.source));
            compare_row(*date, lines, day_kwh, estimate)
        })
        .collect();

    Ok(Json(json!({
        "configured": state.globird_configured,
        "totals": totals_of(&rows),
        "categories": category_totals(&cost_rows),
        "days": rows,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(category: &str, amount_cents: i64, quantity: f64) -> GlobirdCostRow {
        GlobirdCostRow {
            date: "2026-10-09".to_string(),
            category: category.to_string(),
            amount_cents,
            quantity,
        }
    }

    fn date(text: &str) -> NaiveDate {
        NaiveDate::from_str(text).unwrap()
    }

    /// The 9 October fixture day, as stored (cents).
    fn oct_9_lines() -> Vec<GlobirdCostRow> {
        vec![
            line("USAGE", 75, 20.5),
            line("SOLAR", -32, 19.08),
            line("Super Export top up", -118, 14.75),
            line("ZEROHERO Credit", -100, 1.0),
            line("Critical Peak - Export", -35, 0.35),
            line("SUPPLY", 174, 0.0),
        ]
    }

    fn our_cost() -> DayCost {
        DayCost {
            import_kwh: 20.0,
            export_kwh: 19.5,
            load_kwh: 30.0,
            import_cents: 80.0,
            export_cents: 190.0,
            supply_cents: 174.0,
            zero_import_credit_cents: 100.0,
            actual_cents: -36.0,
            baseline_cents: 500.0,
            savings_cents: 536.0,
        }
    }

    #[test]
    fn a_row_is_signed_like_globird_and_diffed() {
        let cost = our_cost();
        let kwh = DayKwh {
            import: Some(20.51),
            export: Some(19.09),
        };
        let row = compare_row(
            date("2026-10-09"),
            &oct_9_lines(),
            kwh,
            Some((&cost, "inverter")),
        );
        assert!(row.complete);

        let g = &row.globird.sums;
        assert_eq!(g.import, 0.75);
        assert_eq!(g.export, -1.85); // 0.32 + 1.18 + 0.35
        assert_eq!(g.supply, 1.74);
        assert_eq!(g.credit, -1.0);
        assert_eq!(g.other, 0.0);
        assert_eq!(g.total, -0.36);
        assert_eq!(g.import_kwh, Some(20.51));
        assert_eq!(row.globird.lines.len(), 6);
        assert_eq!(row.globird.lines[1].amount, -0.32);

        // Our feed-in and credit are positive in DayCost, negative here.
        let o = row.ours.as_ref().unwrap();
        assert_eq!(o.source, "inverter");
        assert_eq!(o.sums.import, 0.8);
        assert_eq!(o.sums.export, -1.9);
        assert_eq!(o.sums.supply, 1.74);
        assert_eq!(o.sums.credit, -1.0);
        assert_eq!(o.sums.total, -0.36);

        let diff = row.diff.unwrap();
        assert_eq!(diff.total, 0.0);
        assert_eq!(diff.import_kwh, Some(-0.51));
        assert_eq!(diff.export_kwh, Some(0.41));
    }

    #[test]
    fn a_row_without_our_estimate_has_no_diff() {
        let lines = vec![line("SUPPLY", 174, 0.0)];
        let row = compare_row(date("2026-10-09"), &lines, DayKwh::default(), None);
        assert!(!row.complete);
        assert_eq!(row.globird.sums.total, 1.74);
        assert_eq!(row.globird.sums.import_kwh, None);
        assert!(row.ours.is_none() && row.diff.is_none());
    }

    #[test]
    fn totals_count_only_days_with_both_sides() {
        let cost = our_cost();
        let kwh = DayKwh {
            import: Some(20.0),
            export: Some(20.0),
        };
        let both = compare_row(
            date("2026-10-09"),
            &oct_9_lines(),
            kwh,
            Some((&cost, "cloud")),
        );
        let only_globird = compare_row(date("2026-10-10"), &oct_9_lines(), kwh, None);
        let totals = totals_of(&[both, only_globird]);
        assert_eq!(totals.days_compared, 1);
        assert_eq!(totals.globird.total, -0.36);
        assert_eq!(totals.ours.total, -0.36);
        assert_eq!(totals.diff.total, 0.0);
        assert_eq!(totals.diff.export_kwh, Some(-0.5));
        assert_eq!(totals_of(&[]).days_compared, 0);
    }

    #[test]
    fn totals_leave_out_incomplete_days() {
        let cost = our_cost();
        let kwh = DayKwh::default();
        let complete = compare_row(
            date("2026-10-09"),
            &oct_9_lines(),
            kwh,
            Some((&cost, "cloud")),
        );
        // GloBird has only posted the supply charge for this day.
        let supply_only = vec![line("SUPPLY", 174, 0.0)];
        let incomplete = compare_row(
            date("2026-10-10"),
            &supply_only,
            kwh,
            Some((&cost, "cloud")),
        );
        assert!(complete.complete && !incomplete.complete);
        assert!(incomplete.diff.is_some());
        let totals = totals_of(&[complete, incomplete]);
        assert_eq!(totals.days_compared, 1);
        assert_eq!(totals.globird.total, -0.36);
        assert_eq!(totals.ours.total, -0.36);
    }

    #[test]
    fn categories_add_up_across_days() {
        let mut rows = oct_9_lines();
        let mut next = oct_9_lines();
        for row in &mut next {
            row.date = "2026-10-10".to_string();
        }
        rows.extend(next);
        let totals = category_totals(&rows);
        let peak = totals
            .iter()
            .find(|c| c.category == "Critical Peak - Export")
            .unwrap();
        assert_eq!(peak.bucket, "export");
        assert_eq!(peak.amount, -0.7);
        assert_eq!(peak.quantity, 0.7);
        assert_eq!(peak.days, 2);
        assert_eq!(totals.len(), 6);
    }

    #[test]
    fn zero_money_is_not_negative_zero() {
        assert!(money(-0.0).is_sign_positive());
        assert!(money(-0.4).is_sign_positive());
    }
}
