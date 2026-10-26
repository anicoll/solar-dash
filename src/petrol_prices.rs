//! Reading a petrol prices CSV file (`petrol_prices_file` in `[teslamate]`).
//!
//! Petrol prices now live in the `petrol_price` table and are edited through
//! `/api/petrol-prices` (see `admin_data.rs`). This module only parses the
//! old file format, so `serve` can import it once.
//!
//! The file has a `from,cents_per_litre` header and one row per change, such
//! as `2026-03-01,199.0`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;

/// The columns the file must start with, in this order.
const HEADER: &str = "from,cents_per_litre";

/// A price change: the first local day it applies and cents per litre.
pub type Change = (NaiveDate, f64);

/// Checks a price the way the config does: finite and more than 0.
pub fn check_price(cents: f64) -> Result<()> {
    // `> 0.0` is false for NaN too.
    if !(cents > 0.0 && cents.is_finite()) {
        bail!("cents_per_litre {cents} must be more than 0");
    }
    Ok(())
}

/// Reads and checks the file. Errors name the file and the line.
pub fn load(path: &Path) -> Result<Vec<Change>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read the petrol prices file {}", path.display()))?;
    parse(&text).with_context(|| format!("invalid petrol prices file {}", path.display()))
}

/// Parses the CSV text. Blank lines and lines starting with `#` are skipped.
/// The changes come back oldest first, whatever order they were written in.
pub fn parse(text: &str) -> Result<Vec<Change>> {
    // The line number is kept so a duplicate can name the earlier line.
    let mut changes: Vec<(usize, Change)> = Vec::new();
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
        let change = parse_line(line).with_context(|| format!("line {number}"))?;
        if let Some((first, _)) = changes.iter().find(|(_, (from, _))| *from == change.0) {
            bail!(
                "line {number}: the date {} is already on line {first}",
                change.0
            );
        }
        changes.push((number, change));
    }
    if !seen_header {
        bail!("the header line `{HEADER}` is missing");
    }
    let mut changes: Vec<Change> = changes.into_iter().map(|(_, change)| change).collect();
    changes.sort_by_key(|(from, _)| *from);
    Ok(changes)
}

fn parse_line(line: &str) -> Result<Change> {
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    if fields.len() != 2 {
        bail!("expected 2 comma-separated fields, found {}", fields.len());
    }
    let from = NaiveDate::parse_from_str(fields[0], "%Y-%m-%d")
        .with_context(|| format!("from {:?} is not a date like 2026-03-01", fields[0]))?;
    let cents: f64 = fields[1]
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite())
        .with_context(|| format!("cents_per_litre {:?} is not a number", fields[1]))?;
    check_price(cents)?;
    Ok((from, cents))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parse_skips_comments_and_blank_lines_and_sorts() {
        let text = "\n# my notes\n from , cents_per_litre \n\n2026-03-01, 199.0\n# cheaper\n2025-01-01,185.5\n";
        assert_eq!(
            parse(text).unwrap(),
            vec![(day("2025-01-01"), 185.5), (day("2026-03-01"), 199.0)]
        );
    }

    #[test]
    fn parse_errors_name_the_line() {
        for (text, expect) in [
            ("", "header line"),
            ("# only a comment\n", "header line"),
            ("date,price\n", "line 1: the header must be"),
            ("from,cents_per_litre\nMarch,199.0\n", "line 2"),
            ("from,cents_per_litre\nMarch,199.0\n", "not a date"),
            ("from,cents_per_litre\n2026-13-01,199.0\n", "not a date"),
            ("from,cents_per_litre\n2026-03-01,abc\n", "not a number"),
            ("from,cents_per_litre\n2026-03-01,nan\n", "not a number"),
            ("from,cents_per_litre\n2026-03-01\n", "found 1"),
            ("from,cents_per_litre\n2026-03-01,1,2\n", "found 3"),
            ("from,cents_per_litre\n2026-03-01,-5\n", "more than 0"),
            ("from,cents_per_litre\n2026-03-01,0\n", "more than 0"),
            (
                "from,cents_per_litre\n2026-03-01,199\n\n2026-03-01,200\n",
                "line 4: the date 2026-03-01 is already on line 2",
            ),
        ] {
            let err = parse(text).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(expect), "{text:?}: {message}");
        }
    }

    #[test]
    fn load_errors_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("petrol.csv");
        let err = load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("petrol.csv"));
        fs::write(&path, "from,cents_per_litre\nbad,1\n").unwrap();
        let message = format!("{:#}", load(&path).unwrap_err());
        assert!(
            message.contains("petrol.csv") && message.contains("line 2"),
            "{message}"
        );
    }
}
