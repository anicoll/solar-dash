//! Past bills: the real billed totals for periods the tariff cannot price
//! (say a retailer with spot prices) or where the meter data has gaps.
//!
//! The bills come from a CSV file (`[bills] file` in the config). This file
//! parses it and works out a [`DayCost`] for every day a bill covers. It is
//! pure: no database, no network, so the maths is tested with plain numbers.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate};

use crate::cost::DayCost;

/// The columns the file must start with, in this order.
const HEADER: &str = "from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,\
                      export_dollars,supply_dollars,note";

/// One past bill. Money is in dollars here, as it is printed on the bill.
#[derive(Debug, Clone, PartialEq)]
pub struct PastBill {
    /// First local day of the bill (inclusive).
    pub from: NaiveDate,
    /// Last local day of the bill (inclusive).
    pub to: NaiveDate,
    /// Dollars billed. A credit is negative.
    pub bill_total: f64,
    /// Rebates (say government relief) added back, so they do not count as
    /// savings from the solar system.
    pub excluded: f64,
    pub import_kwh: f64,
    pub import_dollars: f64,
    pub export_kwh: f64,
    /// Export credits, as a positive number.
    pub export_dollars: f64,
    /// All the daily fixed charges and fees of the period.
    pub supply_dollars: f64,
    /// Free text for the owner; not used in any maths.
    #[allow(dead_code)]
    pub note: String,
}

impl PastBill {
    /// How many days the bill covers.
    pub fn days(&self) -> i64 {
        (self.to - self.from).num_days() + 1
    }

    /// What the period cost before the rebates: `bill_total + excluded`.
    pub fn adjusted_total(&self) -> f64 {
        self.bill_total + self.excluded
    }
}

/// Reads and checks the bills file. Errors name the file and the line.
pub fn load(path: &Path) -> Result<Vec<PastBill>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read the bills file {}", path.display()))?;
    parse(&text).with_context(|| format!("invalid bills file {}", path.display()))
}

/// Parses the CSV text. Blank lines and lines starting with `#` are skipped.
/// The bills come back oldest first.
pub fn parse(text: &str) -> Result<Vec<PastBill>> {
    // The line number is kept so an overlap error can name the line.
    let mut bills: Vec<(usize, PastBill)> = Vec::new();
    let mut seen_header = false;
    for (i, line) in text.lines().enumerate() {
        let number = i + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !seen_header {
            let columns: Vec<&str> = line.split(',').map(str::trim).collect();
            if columns.join(",") != HEADER {
                bail!("line {number}: the header must be `{HEADER}`");
            }
            seen_header = true;
            continue;
        }
        let bill = parse_line(line).with_context(|| format!("line {number}"))?;
        bills.push((number, bill));
    }
    if !seen_header {
        bail!("the header line `{HEADER}` is missing");
    }

    bills.sort_by_key(|(_, bill)| bill.from);
    for pair in bills.windows(2) {
        let ((_, a), (line, b)) = (&pair[0], &pair[1]);
        if b.from <= a.to {
            bail!(
                "line {line}: the period {} to {} overlaps the one ending {}",
                b.from,
                b.to,
                a.to
            );
        }
    }
    Ok(bills.into_iter().map(|(_, bill)| bill).collect())
}

fn parse_line(line: &str) -> Result<PastBill> {
    // The note is last and may hold commas, so it takes whatever is left.
    let fields: Vec<&str> = line.splitn(10, ',').map(str::trim).collect();
    if fields.len() != 10 {
        bail!("expected 10 comma-separated fields, found {}", fields.len());
    }
    let date = |name: &str, text: &str| -> Result<NaiveDate> {
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .with_context(|| format!("{name} {text:?} is not a date like 2026-09-30"))
    };
    let number = |name: &str, text: &str| -> Result<f64> {
        match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(value),
            _ => bail!("{name} {text:?} is not a number"),
        }
    };
    let bill = PastBill {
        from: date("from", fields[0])?,
        to: date("to", fields[1])?,
        bill_total: number("bill_total", fields[2])?,
        excluded: number("excluded", fields[3])?,
        import_kwh: number("import_kwh", fields[4])?,
        import_dollars: number("import_dollars", fields[5])?,
        export_kwh: number("export_kwh", fields[6])?,
        export_dollars: number("export_dollars", fields[7])?,
        supply_dollars: number("supply_dollars", fields[8])?,
        note: fields[9].to_string(),
    };
    if bill.from > bill.to {
        bail!("from {} is after to {}", bill.from, bill.to);
    }
    if bill.import_kwh < 0.0 || bill.export_kwh < 0.0 {
        bail!("import_kwh and export_kwh cannot be negative");
    }
    Ok(bill)
}

/// What our own estimate knows about one day inside a bill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metered {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub load_kwh: f64,
}

/// The cost of every day of `bill`, oldest first.
///
/// `ours` holds the days we have data for. A day without data gets the bill's
/// average daily import and export, and a load estimated as the average daily
/// load of the same calendar month (`monthly_load`, keyed 1 to 12; see
/// `db::average_load_by_month`), or the bill's average daily import when that
/// month has no data at all.
///
/// Each day is priced at the bill's average rates. Because those are only
/// averages, the days rarely add up to the bill exactly. The leftover is
/// shared equally over the days and folded into `import_cents` when the days
/// came out too cheap, or into `export_cents` when too dear, so that
/// `supply + import - export` is still the day's `actual_cents` and the days
/// add up to `adjusted_total` exactly.
pub fn bill_days(
    bill: &PastBill,
    ours: &HashMap<NaiveDate, Metered>,
    monthly_load: &HashMap<u32, f64>,
) -> Vec<(NaiveDate, DayCost)> {
    let days = bill.days() as f64;
    let avg_import = cents_per_kwh(bill.import_dollars, bill.import_kwh);
    let avg_export = cents_per_kwh(bill.export_dollars, bill.export_kwh);
    let supply_day = bill.supply_dollars * 100.0 / days;
    let flat_import = bill.import_kwh / days;
    let flat_export = bill.export_kwh / days;

    let mut out: Vec<(NaiveDate, DayCost)> = bill
        .from
        .iter_days()
        .take_while(|date| *date <= bill.to)
        .map(|date| {
            let m = ours.get(&date).copied().unwrap_or(Metered {
                import_kwh: flat_import,
                export_kwh: flat_export,
                load_kwh: monthly_load
                    .get(&date.month())
                    .copied()
                    .unwrap_or(flat_import),
            });
            let import_cents = m.import_kwh * avg_import;
            let export_cents = m.export_kwh * avg_export;
            let actual_cents = supply_day + import_cents - export_cents;
            let baseline_cents = supply_day + m.load_kwh * avg_import;
            let cost = DayCost {
                import_kwh: m.import_kwh,
                export_kwh: m.export_kwh,
                load_kwh: m.load_kwh,
                import_cents,
                export_cents,
                supply_cents: supply_day,
                zero_import_credit_cents: 0.0,
                actual_cents,
                baseline_cents,
                savings_cents: baseline_cents - actual_cents,
            };
            (date, cost)
        })
        .collect();

    // Share what is left over the days, so they add up to the bill.
    let first_pass: f64 = out.iter().map(|(_, c)| c.actual_cents).sum();
    let share = (bill.adjusted_total() * 100.0 - first_pass) / days;
    for (_, cost) in &mut out {
        if share >= 0.0 {
            cost.import_cents += share;
        } else {
            cost.export_cents -= share;
        }
        cost.actual_cents += share;
        cost.savings_cents -= share;
    }
    out
}

/// Cents per kWh from a dollar amount and the kWh, 0 when there was no energy.
fn cents_per_kwh(dollars: f64, kwh: f64) -> f64 {
    if kwh > 0.0 {
        dollars * 100.0 / kwh
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    const GOOD: &str = "# past bills\n\n\
        from,to,bill_total,excluded,import_kwh,import_dollars,export_kwh,export_dollars,supply_dollars,note\n\
        2026-01-01,2026-01-10,50.00,0,100,40.00,50,5.00,12.00,first\n\
        2026-01-11,2026-01-20,-10.00,5.00,80,30,200,20,12,\n";

    #[test]
    fn parses_comments_and_an_empty_note() {
        let bills = parse(GOOD).unwrap();
        assert_eq!(bills.len(), 2);
        assert_eq!(bills[0].from, date("2026-01-01"));
        assert_eq!(bills[0].days(), 10);
        assert_eq!(bills[0].note, "first");
        assert_eq!(bills[1].bill_total, -10.0);
        assert_eq!(bills[1].adjusted_total(), -5.0);
        assert_eq!(bills[1].note, "");
    }

    #[test]
    fn a_note_may_hold_commas() {
        let text = GOOD.replace(",first", ",paid late, with a fee");
        assert_eq!(parse(&text).unwrap()[0].note, "paid late, with a fee");
    }

    #[test]
    fn overlapping_periods_are_rejected() {
        let text = GOOD.replace("2026-01-11,2026-01-20", "2026-01-10,2026-01-20");
        let err = format!("{:#}", parse(&text).unwrap_err());
        assert!(err.contains("line 5") && err.contains("overlaps"), "{err}");
    }

    #[test]
    fn errors_name_the_line() {
        let text = GOOD.replace("2026-01-11", "2026-13-11");
        let err = format!("{:#}", parse(&text).unwrap_err());
        assert!(err.contains("line 5") && err.contains("from"), "{err}");

        let text = GOOD.replace("100,40.00", "lots,40.00");
        assert!(format!("{:#}", parse(&text).unwrap_err()).contains("line 4"));

        let text = GOOD.replace("2026-01-10,50.00", "2025-12-31,50.00");
        assert!(format!("{:#}", parse(&text).unwrap_err()).contains("after to"));
    }

    #[test]
    fn the_header_is_required() {
        let text = "2026-01-01,2026-01-10,50,0,100,40,50,5,12,\n";
        assert!(parse(text).is_err());
        assert!(parse("# only a comment\n").is_err());
    }

    /// 10 days: average import 40 c/kWh, average export 10 c/kWh, supply 120 c a day.
    fn january_bill() -> PastBill {
        parse(GOOD).unwrap().remove(0)
    }

    fn metered(import_kwh: f64, export_kwh: f64, load_kwh: f64) -> Metered {
        Metered {
            import_kwh,
            export_kwh,
            load_kwh,
        }
    }

    #[test]
    fn the_days_add_up_to_the_adjusted_total() {
        let bill = january_bill();
        // Two days have data of ours; the other eight are filled in.
        let ours = HashMap::from([
            (date("2026-01-02"), metered(3.0, 8.0, 12.0)),
            (date("2026-01-03"), metered(3.0, 8.0, 12.0)),
        ]);
        let days = bill_days(&bill, &ours, &HashMap::new());
        assert_eq!(days.len(), 10);
        let total: f64 = days.iter().map(|(_, c)| c.actual_cents).sum();
        assert!((total - 5000.0).abs() < 1e-6, "{total}");
        for (_, c) in &days {
            // The parts still make up the day.
            let parts = c.supply_cents + c.import_cents - c.export_cents;
            assert!((parts - c.actual_cents).abs() < 1e-9);
            assert!((c.savings_cents - (c.baseline_cents - c.actual_cents)).abs() < 1e-9);
        }
    }

    #[test]
    fn excluded_rebates_are_added_back() {
        let mut bill = january_bill();
        bill.bill_total = 40.0;
        bill.excluded = 10.0;
        let days = bill_days(&bill, &HashMap::new(), &HashMap::new());
        let total: f64 = days.iter().map(|(_, c)| c.actual_cents).sum();
        assert!((total - 5000.0).abs() < 1e-6);
    }

    #[test]
    fn a_negative_leftover_goes_into_export() {
        let mut bill = january_bill();
        bill.bill_total = 0.0;
        let days = bill_days(&bill, &HashMap::new(), &HashMap::new());
        let total: f64 = days.iter().map(|(_, c)| c.actual_cents).sum();
        assert!(total.abs() < 1e-6);
        // First pass: 120 + 400 - 50 = 470 c a day, so 470 c more export credit.
        assert!((days[0].1.export_cents - 520.0).abs() < 1e-9);
        assert!((days[0].1.import_cents - 400.0).abs() < 1e-9);
    }

    #[test]
    fn days_without_data_use_the_average_and_an_estimated_load() {
        let bill = january_bill();
        let ours = HashMap::from([(date("2026-01-02"), metered(3.0, 8.0, 12.0))]);
        let monthly = HashMap::from([(1, 15.0)]);
        let days = bill_days(&bill, &ours, &monthly);

        let with_data = &days[1].1;
        assert_eq!(with_data.import_kwh, 3.0);
        assert_eq!(with_data.load_kwh, 12.0);

        let without = &days[0].1;
        assert_eq!(without.import_kwh, 10.0); // 100 kWh over 10 days
        assert_eq!(without.export_kwh, 5.0);
        assert_eq!(without.load_kwh, 15.0); // January's average
        // Supply 120 c plus 15 kWh at 40 c.
        assert!((without.baseline_cents - 720.0).abs() < 1e-9);

        // With no monthly average, the load falls back to the bill's import.
        let days = bill_days(&bill, &HashMap::new(), &HashMap::new());
        assert_eq!(days[0].1.load_kwh, 10.0);
    }

    #[test]
    fn no_energy_means_a_zero_average_price() {
        let mut bill = january_bill();
        bill.import_kwh = 0.0;
        bill.export_kwh = 0.0;
        let days = bill_days(&bill, &HashMap::new(), &HashMap::new());
        let total: f64 = days.iter().map(|(_, c)| c.actual_cents).sum();
        assert!((total - 5000.0).abs() < 1e-6);
    }
}
