//! Petrol prices kept in a CSV file (`petrol_prices_file` in `[teslamate]`),
//! editable by hand or through `/api/petrol-prices`.
//!
//! The file has a `from,cents_per_litre` header and one row per change, such
//! as `2026-03-01,199.0`. This module parses and writes it, and keeps the
//! parsed prices in a [`PetrolPrices`] store that notices when the file
//! changes on disk. Without a file the store just holds the prices from the
//! `[[teslamate.petrol_price]]` tables, read only.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use tracing::warn;

use crate::ev_cost::PetrolHistory;

/// The columns the file must start with, in this order.
const HEADER: &str = "from,cents_per_litre";

/// The comment line the API writes at the top of the file.
const COMMENT: &str = "# Petrol prices for solar-dash; edit by hand or via /api/petrol-prices";

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

/// The file text for `changes` (which are written in the order given).
pub fn format(changes: &[Change]) -> String {
    let mut text = format!("{COMMENT}\n{HEADER}\n");
    for (from, cents) in changes {
        // `{:?}` keeps the ".0" on whole numbers, as in `195.0`.
        let _ = writeln!(text, "{from},{cents:?}");
    }
    text
}

/// Why a change through the API did not happen.
#[derive(Debug)]
pub enum StoreError {
    /// No `petrol_prices_file`, so there is nowhere to write.
    NotConfigured,
    /// The file on disk does not parse, so it is not overwritten.
    BadFile(String),
    NotFound(NaiveDate),
    Invalid(String),
    Io(anyhow::Error),
}

/// What identifies a version of the file: cheap to read, and changes when the
/// file is edited. The size catches an edit within the clock's granularity.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stamp {
    modified: SystemTime,
    len: u64,
}

/// `None` when the file does not exist.
fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(Stamp {
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            len: meta.len(),
        })),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

#[derive(Debug, Default)]
struct State {
    /// The last good prices, oldest first.
    changes: Vec<Change>,
    /// The version of the file `changes` came from or that failed to parse;
    /// `None` when unknown (the first look, or the file could not be read).
    seen: Option<Option<Stamp>>,
    /// Why the file's current version could not be used, if it could not.
    problem: Option<String>,
}

#[derive(Debug)]
struct Inner {
    /// `petrol_cents_per_litre`, which applies before the first change.
    base: Option<f64>,
    file: Option<PathBuf>,
    state: Mutex<State>,
}

/// The prices shown by `GET /api/petrol-prices`.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// Whether the prices come from a file (and can be changed).
    pub file: bool,
    pub fallback: Option<f64>,
    pub prices: Vec<Change>,
}

/// The petrol prices, shared between requests. Cloning shares the same store.
#[derive(Debug, Clone)]
pub struct PetrolPrices {
    inner: Arc<Inner>,
}

impl Default for PetrolPrices {
    fn default() -> Self {
        Self::fixed(None, Vec::new())
    }
}

impl PetrolPrices {
    /// Prices that come from the config and never change.
    pub fn fixed(base: Option<f64>, mut changes: Vec<Change>) -> Self {
        changes.sort_by_key(|(from, _)| *from);
        Self {
            inner: Arc::new(Inner {
                base,
                file: None,
                state: Mutex::new(State {
                    changes,
                    ..State::default()
                }),
            }),
        }
    }

    /// Prices kept in `file`. A missing file counts as empty; a file that
    /// does not parse is an error, with the line in the message.
    pub fn open(base: Option<f64>, file: &Path) -> Result<Self> {
        let stamp = stamp(file)
            .with_context(|| format!("could not read the petrol prices file {}", file.display()))?;
        let changes = if stamp.is_some() {
            load(file)?
        } else {
            Vec::new()
        };
        Ok(Self {
            inner: Arc::new(Inner {
                base,
                file: Some(file.to_path_buf()),
                state: Mutex::new(State {
                    changes,
                    seen: Some(stamp),
                    problem: None,
                }),
            }),
        })
    }

    /// The prices now. With a file this looks at its size and modified time
    /// and reads it again only when they changed, so hand edits show up on
    /// the next call. A file that does not parse is logged once and the last
    /// good prices stay.
    pub fn current(&self) -> PetrolHistory {
        let mut state = self.inner.state.lock().unwrap();
        self.refresh(&mut state);
        PetrolHistory::new(self.inner.base, state.changes.clone())
    }

    /// The prices for `GET /api/petrol-prices`.
    pub fn view(&self) -> View {
        let mut state = self.inner.state.lock().unwrap();
        self.refresh(&mut state);
        self.view_of(&state)
    }

    /// Adds the change, or replaces the price of that date.
    pub fn put(&self, from: NaiveDate, cents: f64) -> Result<View, StoreError> {
        check_price(cents).map_err(|err| StoreError::Invalid(err.to_string()))?;
        self.write(|changes| {
            match changes.iter_mut().find(|(date, _)| *date == from) {
                Some(change) => change.1 = cents,
                None => changes.push((from, cents)),
            }
            Ok(())
        })
    }

    /// Removes the change that starts on `from`.
    pub fn remove(&self, from: NaiveDate) -> Result<View, StoreError> {
        self.write(|changes| {
            let before = changes.len();
            changes.retain(|(date, _)| *date != from);
            if changes.len() == before {
                return Err(StoreError::NotFound(from));
            }
            Ok(())
        })
    }

    fn view_of(&self, state: &State) -> View {
        View {
            file: self.inner.file.is_some(),
            fallback: self.inner.base,
            prices: state.changes.clone(),
        }
    }

    /// Applies `change` to the current prices and saves them. The lock is
    /// held throughout, and the file is read again first if it changed, so a
    /// hand edit is not lost. The new file is written beside the old one and
    /// renamed over it, so a reader never sees half a file.
    fn write(
        &self,
        change: impl FnOnce(&mut Vec<Change>) -> Result<(), StoreError>,
    ) -> Result<View, StoreError> {
        let Some(path) = &self.inner.file else {
            return Err(StoreError::NotConfigured);
        };
        let mut state = self.inner.state.lock().unwrap();
        self.refresh(&mut state);
        if let Some(problem) = &state.problem {
            return Err(StoreError::BadFile(format!(
                "the petrol prices file cannot be changed until it is fixed: {problem}"
            )));
        }
        let mut changes = state.changes.clone();
        change(&mut changes)?;
        changes.sort_by_key(|(from, _)| *from);
        write_atomic(path, &format(&changes)).map_err(StoreError::Io)?;
        state.changes = changes;
        // Remember the file as we wrote it, so the next look does not re-read.
        state.seen = stamp(path).ok();
        Ok(self.view_of(&state))
    }

    /// Re-reads the file if it changed since the last look.
    fn refresh(&self, state: &mut State) {
        let Some(path) = &self.inner.file else {
            return;
        };
        let now = match stamp(path) {
            Ok(now) => now,
            Err(err) => {
                state.seen = None;
                let message = format!(
                    "could not read the petrol prices file {}: {err}",
                    path.display()
                );
                fail(state, message);
                return;
            }
        };
        if state.seen == Some(now) {
            return;
        }
        state.seen = Some(now);
        let Some(_) = now else {
            // The file was removed: no changes until it comes back.
            state.changes.clear();
            state.problem = None;
            return;
        };
        match load(path) {
            Ok(changes) => {
                state.changes = changes;
                state.problem = None;
            }
            // Keep the last good prices.
            Err(err) => fail(state, format!("{err:#}")),
        }
    }
}

/// Logs a problem with the file once, and remembers it.
fn fail(state: &mut State, message: String) {
    if state.problem.as_deref() != Some(&message) {
        warn!("{message}; keeping the last good petrol prices");
    }
    state.problem = Some(message);
}

/// Writes `text` to `<path>.tmp` in the same directory, then renames it over
/// `path`.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(err)
            .with_context(|| format!("could not write the petrol prices file {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn format_and_parse_round_trip() {
        let changes = vec![(day("2025-01-01"), 185.0), (day("2026-03-01"), 199.9)];
        let text = format(&changes);
        assert_eq!(
            text,
            format!("{COMMENT}\nfrom,cents_per_litre\n2025-01-01,185.0\n2026-03-01,199.9\n")
        );
        assert_eq!(parse(&text).unwrap(), changes);
        // Just a header is an empty list.
        assert_eq!(parse(&format(&[])).unwrap(), vec![]);
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

    #[test]
    fn store_creates_the_file_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("petrol.csv");
        let store = PetrolPrices::open(Some(190.0), &path).unwrap();
        assert!(store.view().prices.is_empty());
        assert_eq!(store.current().on("2026-01-01"), Some(190.0));

        let view = store.put(day("2026-03-01"), 199.0).unwrap();
        assert!(view.file);
        assert_eq!(view.fallback, Some(190.0));
        assert_eq!(view.prices, vec![(day("2026-03-01"), 199.0)]);
        let view = store.put(day("2025-01-01"), 185.0).unwrap();
        assert_eq!(view.prices.len(), 2);
        assert_eq!(view.prices[0].0, day("2025-01-01"));
        // Replacing a date keeps one row.
        let view = store.put(day("2026-03-01"), 201.5).unwrap();
        assert_eq!(view.prices[1], (day("2026-03-01"), 201.5));
        assert_eq!(load(&path).unwrap(), view.prices);
        assert!(!dir.path().join("petrol.csv.tmp").exists());

        assert_eq!(store.current().on("2026-04-01"), Some(201.5));
        assert_eq!(store.current().on("2024-01-01"), Some(190.0));
    }

    #[test]
    fn store_picks_up_hand_edits_and_keeps_good_prices_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("petrol.csv");
        let store = PetrolPrices::open(None, &path).unwrap();
        store.put(day("2026-03-01"), 199.0).unwrap();

        fs::write(
            &path,
            "from,cents_per_litre\n2026-03-01,150.0\n2026-04-01,160.0\n",
        )
        .unwrap();
        assert_eq!(store.current().on("2026-04-02"), Some(160.0));

        // A broken edit keeps the last good prices and refuses API writes.
        fs::write(&path, "from,cents_per_litre\noops\n").unwrap();
        assert_eq!(store.current().on("2026-04-02"), Some(160.0));
        assert!(matches!(
            store.put(day("2026-05-01"), 170.0),
            Err(StoreError::BadFile(_))
        ));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "from,cents_per_litre\noops\n"
        );

        // Fixing it works again, and a hand edit is not lost by a write.
        fs::write(&path, "from,cents_per_litre\n2026-06-01,171.0\n").unwrap();
        let view = store.put(day("2026-07-01"), 172.0).unwrap();
        assert_eq!(view.prices.len(), 2);
        assert_eq!(view.prices[0], (day("2026-06-01"), 171.0));

        // Removing the file empties the list.
        fs::remove_file(&path).unwrap();
        assert!(store.view().prices.is_empty());
    }

    #[test]
    fn store_removes_dates_and_rejects_bad_prices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("petrol.csv");
        let store = PetrolPrices::open(None, &path).unwrap();
        store.put(day("2026-03-01"), 199.0).unwrap();
        assert!(matches!(
            store.remove(day("2026-03-02")),
            Err(StoreError::NotFound(_))
        ));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                store.put(day("2026-03-05"), bad),
                Err(StoreError::Invalid(_))
            ));
        }
        assert!(store.remove(day("2026-03-01")).unwrap().prices.is_empty());
        assert_eq!(load(&path).unwrap(), vec![]);
    }

    #[test]
    fn fixed_prices_are_read_only() {
        let store = PetrolPrices::fixed(Some(190.0), vec![(day("2026-03-01"), 199.0)]);
        let view = store.view();
        assert!(!view.file);
        assert_eq!(view.prices.len(), 1);
        assert!(matches!(
            store.put(day("2026-04-01"), 200.0),
            Err(StoreError::NotConfigured)
        ));
        assert!(matches!(
            store.remove(day("2026-03-01")),
            Err(StoreError::NotConfigured)
        ));
    }

    #[test]
    fn open_fails_on_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("petrol.csv");
        fs::write(&path, "nope\n").unwrap();
        assert!(PetrolPrices::open(None, &path).is_err());
    }
}
