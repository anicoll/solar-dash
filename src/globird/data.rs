//! GloBird's cost and usage data as typed rows, and the pure steps from the
//! portal's JSON to one [`DayRecord`] per day, ready to save.
//!
//! Nothing here touches the network or the database, so it is all unit tested
//! with real (trimmed) replies from `testdata/`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use chrono::{Days, NaiveDate};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::service::DateRange;

/// The longest date range one portal call may ask for, in days.
pub const MAX_WINDOW_DAYS: u64 = 31;

/// One `CostDetail` row: what one charge category cost on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct CostRow {
    pub date: NaiveDate,
    /// GloBird's own text ("USAGE", "SUPPLY", "ZEROHERO Credit", ...). It is
    /// never interpreted, because new categories can turn up.
    pub category: String,
    /// Dollars (GST included) turned into whole cents. Negative is a credit.
    pub amount_cents: i64,
    /// kWh, or 1 for a ZeroHero credit, or 0 for SUPPLY.
    pub quantity: f64,
    pub meter_read_missing: bool,
}

/// One smart meter read row: the usage of one charge period on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub date: NaiveDate,
    /// The meter register, like "E7" or "B7".
    pub suffix: String,
    /// Text such as "Peak Usage"; empty if the portal sent none.
    pub charge_type: String,
    pub category_code: Option<String>,
    pub kwh: f64,
    pub quality: Option<String>,
    /// The whole day's five-minute values. Every row of the same date and
    /// suffix is supposed to repeat the same array.
    pub intervals: Vec<f64>,
}

// The `Raw...` types are the JSON as the portal sends it. Missing or null
// fields become `None` here, and unknown fields are ignored (serde's default),
// so a small change on GloBird's side does not break parsing.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCost {
    date: String,
    charge_category: String,
    amount: Option<f64>,
    quantity: Option<f64>,
    is_meter_read_missing_on_date: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawUsage {
    read_date: String,
    suffix: String,
    charge_type: Option<String>,
    charge_category_code: Option<String>,
    usage: Option<f64>,
    min_quality_method: Option<String>,
    usage_array: Option<Vec<f64>>,
}

/// Reads "2026/10/09" (what the portal sends) or "2026-10-09".
fn parse_date(text: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y/%m/%d")
        .or_else(|_| NaiveDate::parse_from_str(text, "%Y-%m-%d"))
        .map_err(|_| anyhow!("{text:?} is not a date like 2026/10/09"))
}

/// Turns the `data` of a reply into a list of rows. `null` (or a missing
/// `data`) means no rows.
fn rows_of<T: DeserializeOwned>(data: Value) -> Result<Vec<T>> {
    if data.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value(data).context("the portal's rows are not in the expected shape")
}

/// Dollars to whole cents, rounded to the nearest.
fn dollars_to_cents(dollars: f64) -> i64 {
    (dollars * 100.0).round() as i64
}

/// Reads the `data` of a `CostDetail` reply.
pub fn parse_cost_rows(data: Value) -> Result<Vec<CostRow>> {
    rows_of::<RawCost>(data)?
        .into_iter()
        .map(|raw| {
            Ok(CostRow {
                date: parse_date(&raw.date)?,
                category: raw.charge_category,
                amount_cents: dollars_to_cents(raw.amount.unwrap_or(0.0)),
                quantity: raw.quantity.unwrap_or(0.0),
                meter_read_missing: raw.is_meter_read_missing_on_date.unwrap_or(false),
            })
        })
        .collect()
}

/// Reads the `data` of a smart meter read reply.
pub fn parse_usage_rows(data: Value) -> Result<Vec<UsageRow>> {
    rows_of::<RawUsage>(data)?
        .into_iter()
        .map(|raw| {
            Ok(UsageRow {
                date: parse_date(&raw.read_date)?,
                suffix: raw.suffix,
                charge_type: raw.charge_type.unwrap_or_default(),
                category_code: raw.charge_category_code,
                kwh: raw.usage.unwrap_or(0.0),
                quality: raw.min_quality_method,
                intervals: raw.usage_array.unwrap_or_default(),
            })
        })
        .collect()
}

/// Which way a meter register measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Import,
    Export,
}

impl Direction {
    /// A suffix starting with "E" is import and one starting with "B" is
    /// export. Anything else is unknown.
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        match suffix.trim().chars().next()?.to_ascii_uppercase() {
            'E' => Some(Self::Import),
            'B' => Some(Self::Export),
            _ => None,
        }
    }

    /// The text stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Export => "export",
        }
    }
}

/// One cost category on one day (the day is in the [`DayRecord`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CostLine {
    pub category: String,
    pub amount_cents: i64,
    pub quantity: f64,
    pub meter_read_missing: bool,
}

/// The kWh of one charge period on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageLine {
    pub suffix: String,
    pub charge_type: String,
    pub category_code: Option<String>,
    pub kwh: f64,
    pub quality: Option<String>,
}

/// The five-minute readings of one register on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct IntervalSeries {
    pub suffix: String,
    pub direction: Direction,
    /// The sum of `intervals`.
    pub kwh_total: f64,
    pub intervals: Vec<f64>,
}

/// Everything we know about one local day. Saving it replaces whatever was
/// stored for that day.
#[derive(Debug, Clone, PartialEq)]
pub struct DayRecord {
    pub date: NaiveDate,
    pub cost: Vec<CostLine>,
    pub usage: Vec<UsageLine>,
    pub intervals: Vec<IntervalSeries>,
}

impl DayRecord {
    fn new(date: NaiveDate) -> Self {
        Self {
            date,
            cost: Vec::new(),
            usage: Vec::new(),
            intervals: Vec::new(),
        }
    }

    /// Whether GloBird has finished posting this day: it has a cost line
    /// other than SUPPLY (which is posted first).
    pub fn is_complete(&self) -> bool {
        self.cost.iter().any(|line| !is_supply(&line.category))
    }
}

/// Whether a cost category is the daily supply charge. `db.rs` repeats this
/// rule in SQL when it looks for the newest complete day.
pub fn is_supply(category: &str) -> bool {
    category.trim().eq_ignore_ascii_case("SUPPLY")
}

/// Groups rows into one record per day, oldest first.
///
/// - Cost rows with the same day and category are added together.
/// - Usage rows with the same day, suffix and charge type are added together.
/// - Each register's five-minute array is taken once per day: the one most of
///   its rows carry, since the portal (nearly always) repeats it. A suffix that
///   is neither import nor export has no interval series (its per-period
///   usage is still kept).
pub fn group_days(cost: &[CostRow], usage: &[UsageRow]) -> Vec<DayRecord> {
    let mut days: BTreeMap<NaiveDate, DayRecord> = BTreeMap::new();
    // Every non-empty interval array seen for a register on a day.
    let mut candidates: BTreeMap<(NaiveDate, &str), Vec<&Vec<f64>>> = BTreeMap::new();

    for row in cost {
        let day = days
            .entry(row.date)
            .or_insert_with(|| DayRecord::new(row.date));
        match day.cost.iter_mut().find(|l| l.category == row.category) {
            Some(line) => {
                line.amount_cents += row.amount_cents;
                line.quantity += row.quantity;
                line.meter_read_missing |= row.meter_read_missing;
            }
            None => day.cost.push(CostLine {
                category: row.category.clone(),
                amount_cents: row.amount_cents,
                quantity: row.quantity,
                meter_read_missing: row.meter_read_missing,
            }),
        }
    }

    for row in usage {
        let day = days
            .entry(row.date)
            .or_insert_with(|| DayRecord::new(row.date));
        let same = |l: &&mut UsageLine| l.suffix == row.suffix && l.charge_type == row.charge_type;
        match day.usage.iter_mut().find(same) {
            Some(line) => line.kwh += row.kwh,
            None => day.usage.push(UsageLine {
                suffix: row.suffix.clone(),
                charge_type: row.charge_type.clone(),
                category_code: row.category_code.clone(),
                kwh: row.kwh,
                quality: row.quality.clone(),
            }),
        }

        if Direction::from_suffix(&row.suffix).is_some() && !row.intervals.is_empty() {
            candidates
                .entry((row.date, row.suffix.as_str()))
                .or_default()
                .push(&row.intervals);
        }
    }

    // Each row normally repeats the register's full-day array, but not always:
    // on 2026-10-09 the "Critical Peak - Export" row had its first six
    // intervals zeroed. Keep the array most rows agree on (the first on a tie).
    for ((date, suffix), arrays) in candidates {
        let Some(direction) = Direction::from_suffix(suffix) else {
            continue;
        };
        let count = |a: &Vec<f64>| arrays.iter().filter(|b| **b == a).count();
        let mut best = arrays[0];
        for &array in &arrays[1..] {
            if count(array) > count(best) {
                best = array;
            }
        }
        if let Some(day) = days.get_mut(&date) {
            day.intervals.push(IntervalSeries {
                suffix: suffix.to_string(),
                direction,
                kwh_total: best.iter().sum(),
                intervals: best.clone(),
            });
        }
    }

    days.into_values().collect()
}

/// Splits `from..=to` into consecutive ranges of at most [`MAX_WINDOW_DAYS`]
/// days, oldest first. Both ends are inclusive, and the ranges neither overlap
/// nor leave a gap. Empty if `from` is after `to`.
pub fn split_windows(from: NaiveDate, to: NaiveDate) -> Vec<DateRange> {
    let mut windows = Vec::new();
    let mut start = from;
    while start <= to {
        // The last day of a window of exactly MAX_WINDOW_DAYS days.
        let end = (start + Days::new(MAX_WINDOW_DAYS - 1)).min(to);
        windows.push(DateRange {
            from: start,
            to: end,
        });
        start = end + Days::new(1);
    }
    windows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COST_JSON: &str = include_str!("testdata/cost_detail.json");
    const SMART_JSON: &str = include_str!("testdata/smart_meter_read.json");

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// The `data` part of a saved reply, which is what the client hands back.
    fn data_of(reply: &str) -> Value {
        let mut reply: Value = serde_json::from_str(reply).unwrap();
        reply["data"].take()
    }

    fn cost_rows() -> Vec<CostRow> {
        parse_cost_rows(data_of(COST_JSON)).unwrap()
    }

    fn usage_rows() -> Vec<UsageRow> {
        parse_usage_rows(data_of(SMART_JSON)).unwrap()
    }

    fn cost_row(day: u32, category: &str, cents: i64) -> CostRow {
        CostRow {
            date: date(2026, 10, day),
            category: category.to_string(),
            amount_cents: cents,
            quantity: 1.0,
            meter_read_missing: false,
        }
    }

    #[test]
    fn parses_the_cost_testdata() {
        let rows = cost_rows();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].date, date(2026, 10, 9));
        assert_eq!(rows[0].category, "USAGE");
        assert_eq!(rows[0].amount_cents, 75);
        assert_eq!(rows[0].quantity, 20.5);
        assert!(!rows[0].meter_read_missing);
        // A credit is negative, and the ZeroHero quantity is 1.
        let zero_hero = rows.iter().find(|r| r.category == "ZEROHERO Credit");
        let zero_hero = zero_hero.unwrap();
        assert_eq!(zero_hero.amount_cents, -100);
        assert_eq!(zero_hero.quantity, 1.0);
        let supply = rows.iter().find(|r| r.category == "SUPPLY").unwrap();
        assert_eq!((supply.amount_cents, supply.quantity), (174, 0.0));
    }

    #[test]
    fn parses_the_usage_testdata() {
        let rows = usage_rows();
        assert_eq!(rows.len(), 15);
        let first = &rows[0];
        assert_eq!(first.date, date(2026, 10, 9));
        assert_eq!(first.suffix, "B7");
        assert_eq!(first.charge_type, "Solar/Generation Feed in (11pm-4pm)");
        assert_eq!(first.category_code.as_deref(), Some("SOLAR"));
        assert_eq!(first.kwh, 2.848);
        assert_eq!(first.quality.as_deref(), Some("A"));
        assert_eq!(first.intervals.len(), 288);
    }

    #[test]
    fn tolerates_extra_missing_and_null_fields() {
        let cost = parse_cost_rows(json!([
            {"date": "2026/10/09", "chargeCategory": "USAGE", "somethingNew": [1, 2]},
            {"date": "2026-10-10", "chargeCategory": "SOLAR", "amount": null,
             "quantity": null, "isMeterReadMissingOnDate": true},
        ]))
        .unwrap();
        assert_eq!(cost[0].amount_cents, 0);
        assert_eq!(cost[0].quantity, 0.0);
        assert!(!cost[0].meter_read_missing);
        assert_eq!(cost[1].date, date(2026, 10, 10));
        assert!(cost[1].meter_read_missing);

        let usage = parse_usage_rows(json!([
            {"readDate": "2026/10/09", "suffix": "E7", "extra": true},
        ]))
        .unwrap();
        assert_eq!(usage[0].charge_type, "");
        assert_eq!(usage[0].category_code, None);
        assert_eq!(usage[0].kwh, 0.0);
        assert!(usage[0].intervals.is_empty());
    }

    #[test]
    fn empty_and_bad_replies() {
        assert!(parse_cost_rows(Value::Null).unwrap().is_empty());
        assert!(parse_cost_rows(json!([])).unwrap().is_empty());
        assert!(parse_usage_rows(json!([])).unwrap().is_empty());
        // A row we cannot place on a day is an error, not a guess.
        let err = parse_cost_rows(json!([{"date": "soon", "chargeCategory": "X"}])).unwrap_err();
        assert!(format!("{err:#}").contains("soon"));
        assert!(parse_cost_rows(json!({"not": "a list"})).is_err());
    }

    #[test]
    fn dollars_round_to_the_nearest_cent() {
        assert_eq!(dollars_to_cents(0.75), 75);
        assert_eq!(dollars_to_cents(-1.18), -118);
        // 1.005 * 100 is 100.49999999999999 in floating point: still 100.
        assert_eq!(dollars_to_cents(1.005), 100);
        // 0.29 * 100 is 28.999999999999996: rounds up, not down to 28.
        assert_eq!(dollars_to_cents(0.29), 29);
        assert_eq!(dollars_to_cents(-0.29), -29);
    }

    #[test]
    fn groups_the_testdata_by_day() {
        let days = group_days(&cost_rows(), &usage_rows());
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, date(2026, 10, 9));
        assert_eq!(days[1].date, date(2026, 10, 17));
        assert_eq!(days[0].cost.len(), 6);
        assert_eq!(days[1].cost.len(), 4);
        // Charge periods: 4 export and 4 import on the 9th, 3 and 4 on the 17th.
        assert_eq!(days[0].usage.len(), 8);
        assert_eq!(days[1].usage.len(), 7);
        assert!(days.iter().all(DayRecord::is_complete));
    }

    #[test]
    fn keeps_one_interval_array_per_register_and_day() {
        let days = group_days(&[], &usage_rows());
        // Fifteen rows, but only two registers per day.
        for day in &days {
            let suffixes: Vec<&str> = day.intervals.iter().map(|s| s.suffix.as_str()).collect();
            assert_eq!(suffixes, ["B7", "E7"], "{}", day.date);
        }
        let day = &days[0];
        let export = day.intervals.iter().find(|s| s.suffix == "B7").unwrap();
        let import = day.intervals.iter().find(|s| s.suffix == "E7").unwrap();
        assert_eq!(export.direction, Direction::Export);
        assert_eq!(import.direction, Direction::Import);
        assert_eq!(export.intervals.len(), 288);
        assert_eq!(import.intervals.len(), 288);
        // The array is the one most of the register's rows share, and the total is its sum.
        let first_export = usage_rows()
            .into_iter()
            .find(|r| r.date == day.date && r.suffix == "B7")
            .unwrap();
        assert_eq!(export.intervals, first_export.intervals);
        assert!(
            (export.kwh_total - 19.085).abs() < 0.01,
            "{}",
            export.kwh_total
        );
        assert!(
            (import.kwh_total - 20.508).abs() < 0.01,
            "{}",
            import.kwh_total
        );
    }

    #[test]
    fn interval_array_skips_empty_rows_and_unknown_suffixes() {
        let row = |suffix: &str, charge: &str, array: Vec<f64>| UsageRow {
            date: date(2026, 10, 9),
            suffix: suffix.to_string(),
            charge_type: charge.to_string(),
            category_code: None,
            kwh: 1.0,
            quality: None,
            intervals: array,
        };
        let days = group_days(
            &[],
            &[
                row("E7", "Peak", vec![]),
                row("E7", "Shoulder", vec![0.5, 0.25]),
                row("E7", "Offpeak", vec![9.0]),
                row("X1", "Other", vec![1.0]),
            ],
        );
        let day = &days[0];
        assert_eq!(day.intervals.len(), 1);
        assert_eq!(day.intervals[0].intervals, [0.5, 0.25]);
        assert_eq!(day.intervals[0].kwh_total, 0.75);
        // The unknown register still has its per-period usage.
        assert_eq!(day.usage.len(), 4);
    }

    #[test]
    fn duplicate_rows_are_added_together() {
        let mut second = cost_row(9, "USAGE", 30);
        second.quantity = 2.0;
        second.meter_read_missing = true;
        let days = group_days(&[cost_row(9, "USAGE", 50), second], &[]);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].cost.len(), 1);
        let line = &days[0].cost[0];
        assert_eq!(line.amount_cents, 80);
        assert_eq!(line.quantity, 3.0);
        assert!(line.meter_read_missing);

        let usage_row = |kwh| UsageRow {
            date: date(2026, 10, 9),
            suffix: "E7".into(),
            charge_type: "Peak Usage".into(),
            category_code: None,
            kwh,
            quality: None,
            intervals: vec![],
        };
        let days = group_days(&[], &[usage_row(1.5), usage_row(0.5)]);
        assert_eq!(days[0].usage.len(), 1);
        assert_eq!(days[0].usage[0].kwh, 2.0);
    }

    #[test]
    fn days_come_out_oldest_first() {
        let rows = [cost_row(12, "USAGE", 1), cost_row(3, "USAGE", 1)];
        let dates: Vec<NaiveDate> = group_days(&rows, &[]).iter().map(|d| d.date).collect();
        assert_eq!(dates, [date(2026, 10, 3), date(2026, 10, 12)]);
    }

    #[test]
    fn a_day_is_complete_once_it_has_more_than_supply() {
        let day = |categories: &[&str]| {
            let rows: Vec<CostRow> = categories.iter().map(|c| cost_row(9, c, 1)).collect();
            group_days(&rows, &[]).remove(0)
        };
        assert!(!day(&["SUPPLY"]).is_complete());
        assert!(!day(&["supply"]).is_complete());
        assert!(day(&["SUPPLY", "USAGE"]).is_complete());
        // Any other category counts, even one we have never seen.
        assert!(day(&["SUPPLY", "Brand New Credit"]).is_complete());
        assert!(!DayRecord::new(date(2026, 10, 9)).is_complete());
    }

    #[test]
    fn direction_comes_from_the_suffix() {
        assert_eq!(Direction::from_suffix("E7"), Some(Direction::Import));
        assert_eq!(Direction::from_suffix("e1"), Some(Direction::Import));
        assert_eq!(Direction::from_suffix("B7"), Some(Direction::Export));
        assert_eq!(Direction::from_suffix("K1"), None);
        assert_eq!(Direction::from_suffix(""), None);
        assert_eq!(Direction::Import.as_str(), "import");
        assert_eq!(Direction::Export.as_str(), "export");
    }

    #[test]
    fn windows_cover_the_range_with_no_gaps_or_overlaps() {
        // One day, and a range that fits in one window.
        assert_eq!(
            split_windows(date(2026, 10, 1), date(2026, 10, 1)),
            [DateRange {
                from: date(2026, 10, 1),
                to: date(2026, 10, 1)
            }]
        );
        assert_eq!(
            split_windows(date(2026, 10, 1), date(2026, 10, 31)).len(),
            1
        );
        // 32 days need two windows: 31 + 1.
        let two = split_windows(date(2026, 10, 1), date(2026, 11, 1));
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].to, date(2026, 10, 31));
        assert_eq!(two[1].from, date(2026, 11, 1));
        // Nothing for a backwards range.
        assert!(split_windows(date(2026, 10, 2), date(2026, 10, 1)).is_empty());

        // A long range (a leap year, to be mean): every day exactly once.
        let (from, to) = (date(2024, 1, 1), date(2026, 10, 22));
        let windows = split_windows(from, to);
        assert_eq!(windows[0].from, from);
        assert_eq!(windows.last().unwrap().to, to);
        for window in &windows {
            let days = (window.to - window.from).num_days() + 1;
            assert!((1..=31).contains(&days), "{window:?}");
        }
        for pair in windows.windows(2) {
            assert_eq!(pair[1].from, pair[0].to + Days::new(1), "{pair:?}");
        }
    }
}
