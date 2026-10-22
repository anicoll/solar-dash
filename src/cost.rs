//! Electricity cost calculation. Pure functions only: no database, no network,
//! so every rule here can be tested with plain numbers.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Timelike};
use chrono_tz::Tz;

use crate::config;

/// A daily time window as minutes since local midnight.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Window {
    /// Inclusive.
    start: u32,
    /// Exclusive. Smaller than `start` when the window wraps past midnight.
    end: u32,
}

impl Window {
    fn contains(&self, minute: u32) -> bool {
        if self.start < self.end {
            self.start <= minute && minute < self.end
        } else {
            // Wraps past midnight: the late evening part or the early morning part.
            minute >= self.start || minute < self.end
        }
    }

    /// Whether `minute` falls in the last `minutes` of the window, counting
    /// the closing instant itself. Works across midnight.
    fn ends_within(&self, minute: u32, minutes: u32) -> bool {
        let to_end = (self.end + MINUTES_PER_DAY - minute % MINUTES_PER_DAY) % MINUTES_PER_DAY;
        to_end <= minutes
    }
}

const MINUTES_PER_DAY: u32 = 24 * 60;

/// How close to the end of the credit window the data must reach before the
/// window counts as finished. One interval's worth, so a small gap is fine.
const WINDOW_FINISHED_MINUTES: u32 = 15;

/// A price that applies inside a window.
#[derive(Debug, Clone, Copy)]
struct PricedWindow {
    window: Window,
    cents: f64,
}

/// The first window containing `minute` wins; otherwise the default price.
fn price_at(windows: &[PricedWindow], default_cents: f64, minute: u32) -> f64 {
    windows
        .iter()
        .find(|w| w.window.contains(minute))
        .map_or(default_cents, |w| w.cents)
}

#[derive(Debug, Clone, Copy)]
struct ZeroImportCredit {
    window: Window,
    max_kwh: f64,
    credit_cents: f64,
}

/// A tariff with its times already parsed to minutes, so lookups are cheap
/// and cannot fail. Build it from the config with [`Tariff::from_config`].
#[derive(Debug, Clone)]
pub struct Tariff {
    supply_cents_per_day: f64,
    default_import_cents: f64,
    import: Vec<PricedWindow>,
    default_export_cents: f64,
    export: Vec<PricedWindow>,
    zero_import_credit: Option<ZeroImportCredit>,
    /// Identifies these exact rates; see [`Tariff::key`].
    key: String,
}

fn windows_of(list: &[PricedWindow]) -> Vec<(u32, u32, f64)> {
    list.iter()
        .map(|w| (w.window.start, w.window.end, w.cents))
        .collect()
}

/// Parse "HH:MM" start and end strings into a [`Window`].
fn parse_window(from: &str, to: &str) -> Result<Window> {
    Ok(Window {
        start: config::parse_hhmm(from)?,
        end: config::parse_hhmm(to)?,
    })
}

fn parse_priced(windows: &[config::PriceWindow]) -> Result<Vec<PricedWindow>> {
    windows
        .iter()
        .map(|w| {
            Ok(PricedWindow {
                window: parse_window(&w.from, &w.to)?,
                cents: w.cents,
            })
        })
        .collect()
}

impl Tariff {
    /// Convert the config section. `Config::load` already checks the times, so
    /// an error here means the config was built some other way.
    pub fn from_config(cfg: &config::Tariff) -> Result<Self> {
        let zero_import_credit = cfg
            .zero_import_credit
            .as_ref()
            .map(|c| {
                Ok::<_, anyhow::Error>(ZeroImportCredit {
                    window: parse_window(&c.from, &c.to)?,
                    max_kwh: c.max_kwh,
                    credit_cents: c.credit_cents,
                })
            })
            .transpose()
            .context("invalid zero_import_credit in [tariff]")?;
        Ok(Self {
            supply_cents_per_day: cfg.supply_cents_per_day,
            default_import_cents: cfg.default_import_cents,
            import: parse_priced(&cfg.import).context("invalid import window in [tariff]")?,
            default_export_cents: cfg.default_export_cents,
            export: parse_priced(&cfg.export).context("invalid export window in [tariff]")?,
            zero_import_credit,
            key: serde_json::to_string(cfg).context("could not describe the [tariff] section")?,
        })
    }

    /// A string that is the same for the same rates and differs when any rate
    /// or window changes (it is the config section written as JSON). Stored
    /// costs are tagged with it, so editing config.toml makes them stale.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The zero-import credit window as (start, end) minutes since local
    /// midnight, or `None` when the plan has no such credit.
    pub fn credit_window_minutes(&self) -> Option<(u32, u32)> {
        let credit = self.zero_import_credit.as_ref()?;
        Some((credit.window.start, credit.window.end))
    }

    /// Whether this local time (minutes since midnight) is inside the credit window.
    pub fn in_credit_window(&self, minute_of_day: u32) -> bool {
        self.zero_import_credit
            .as_ref()
            .is_some_and(|credit| credit.window.contains(minute_of_day))
    }

    /// The credit's value in cents, or `None` when the plan has no such credit.
    pub fn credit_cents(&self) -> Option<f64> {
        self.zero_import_credit.as_ref().map(|c| c.credit_cents)
    }

    /// When free import starts (minutes since local midnight): the start of
    /// the first import window priced at 0 c. `None` if no import is free.
    pub fn free_import_start(&self) -> Option<u32> {
        self.import
            .iter()
            .find(|w| w.cents <= 0.0)
            .map(|w| w.window.start)
    }

    /// The plan's import windows as `(start, end, cents)`, minutes since local
    /// midnight, in config order (the first match wins). `end` is smaller than
    /// `start` when the window wraps past midnight.
    pub fn import_windows(&self) -> Vec<(u32, u32, f64)> {
        windows_of(&self.import)
    }

    /// The plan's feed-in windows, like [`Tariff::import_windows`].
    pub fn export_windows(&self) -> Vec<(u32, u32, f64)> {
        windows_of(&self.export)
    }

    /// Daily supply charge in cents.
    pub fn supply_cents_per_day(&self) -> f64 {
        self.supply_cents_per_day
    }

    /// Import price outside every import window, in cents.
    pub fn default_import_cents(&self) -> f64 {
        self.default_import_cents
    }

    /// Feed-in price outside every export window, in cents.
    pub fn default_export_cents(&self) -> f64 {
        self.default_export_cents
    }

    /// The most grid import (kWh) in the credit window that still earns the credit.
    pub fn credit_max_kwh(&self) -> Option<f64> {
        self.zero_import_credit.as_ref().map(|c| c.max_kwh)
    }

    /// Cents per kWh paid to import at this local time (minutes since midnight).
    pub fn import_cents_at(&self, minute_of_day: u32) -> f64 {
        price_at(&self.import, self.default_import_cents, minute_of_day)
    }

    /// Cents per kWh earned for exporting at this local time.
    pub fn export_cents_at(&self, minute_of_day: u32) -> f64 {
        price_at(&self.export, self.default_export_cents, minute_of_day)
    }
}

/// Which import band a minute of the day falls in. The tariff does not name
/// its bands, so they are told apart by price (see [`Tariff::import_band_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportBand {
    Free,
    Shoulder,
    Peak,
}

/// Which export band a minute of the day falls in (see [`Tariff::export_band_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportBand {
    Super,
    Peak,
    Other,
}

impl Tariff {
    /// The role of the import price at this minute, judged by price against
    /// this tariff's own rates: nothing to pay is `Free`, dearer than the
    /// default import price is `Peak`, anything else is `Shoulder`.
    pub fn import_band_at(&self, minute_of_day: u32) -> ImportBand {
        self.import_band_for(self.import_cents_at(minute_of_day))
    }

    /// The role of an import price `cents` against this tariff (see [`Tariff::import_band_at`]).
    pub fn import_band_for(&self, cents: f64) -> ImportBand {
        if cents <= 0.0 {
            ImportBand::Free
        } else if cents > self.default_import_cents {
            ImportBand::Peak
        } else {
            ImportBand::Shoulder
        }
    }

    /// The role of the feed-in price at this minute: a rate no better than
    /// the default export price is `Other`; of the rates above the default the
    /// highest is `Super`, but only when a lower one exists too (a lone
    /// better-than-default rate is `Peak`).
    pub fn export_band_at(&self, minute_of_day: u32) -> ExportBand {
        self.export_band_for(self.export_cents_at(minute_of_day))
    }

    /// The role of a feed-in price `cents` against this tariff (see [`Tariff::export_band_at`]).
    pub fn export_band_for(&self, cents: f64) -> ExportBand {
        if cents <= self.default_export_cents {
            return ExportBand::Other;
        }
        let better = |c: f64| c > self.default_export_cents;
        let top = self
            .export
            .iter()
            .map(|w| w.cents)
            .filter(|&c| better(c))
            .fold(f64::MIN, f64::max);
        let has_lower = self.export.iter().any(|w| better(w.cents) && w.cents < top);
        if has_lower && cents >= top {
            ExportBand::Super
        } else {
            ExportBand::Peak
        }
    }
}

/// The plans over time: past ones, each with the last day it applied, then the
/// current one. Build it with [`TariffSchedule::from_config`].
#[derive(Debug, Clone)]
pub struct TariffSchedule {
    /// Oldest first, with strictly increasing `until` days (inclusive).
    previous: Vec<(NaiveDate, Tariff)>,
    current: Tariff,
}

impl TariffSchedule {
    /// A schedule with one plan for every day.
    pub fn single(current: Tariff) -> Self {
        Self {
            previous: Vec::new(),
            current,
        }
    }

    /// Convert the config. `Config::load` already checks the dates and order,
    /// so an error here means the config was built some other way.
    pub fn from_config(
        current: &config::Tariff,
        previous: &[config::PreviousTariff],
    ) -> Result<Self> {
        let previous = previous
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let until = config::parse_date(&p.until)
                    .with_context(|| format!("until in [[previous_tariff]] #{}", i + 1))?;
                let tariff = Tariff::from_config(&p.tariff)
                    .with_context(|| format!("invalid [[previous_tariff]] #{}", i + 1))?;
                Ok((until, tariff))
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            previous.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "[[previous_tariff]] `until` days must increase"
        );
        Ok(Self {
            previous,
            current: Tariff::from_config(current)?,
        })
    }

    /// The plan in effect on this local day: the first previous plan whose
    /// `until` is on or after it, otherwise the current one.
    pub fn for_date(&self, date: NaiveDate) -> &Tariff {
        self.previous
            .iter()
            .find(|(until, _)| *until >= date)
            .map_or(&self.current, |(_, tariff)| tariff)
    }
}

/// Energy over one span of time, in kWh. Times are unix seconds.
#[derive(Debug, Clone, Copy)]
pub struct Interval {
    pub start_ts: i64,
    pub end_ts: i64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub load_kwh: f64,
}

/// What one day cost. All money is in cents, all energy in kWh.
#[derive(Debug, Clone, PartialEq)]
pub struct DayCost {
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub load_kwh: f64,
    pub import_cents: f64,
    /// Feed-in credit, as a positive number.
    pub export_cents: f64,
    pub supply_cents: f64,
    pub zero_import_credit_cents: f64,
    /// What the day really cost: supply + import - export - credit.
    pub actual_cents: f64,
    /// What it would have cost with no solar or battery: all the load bought
    /// from the grid at the import price, plus supply.
    pub baseline_cents: f64,
    /// `baseline_cents - actual_cents`.
    pub savings_cents: f64,
}

/// Grid energy in kWh split by tariff band. See [`grid_split`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GridSplit {
    pub import_free_kwh: f64,
    pub import_shoulder_kwh: f64,
    pub import_peak_kwh: f64,
    pub export_super_kwh: f64,
    pub export_peak_kwh: f64,
    pub export_other_kwh: f64,
}

/// The local minute an interval is priced at: its midpoint.
fn interval_minute(tz: Tz, iv: &Interval) -> u32 {
    // Sum then halve, so we cannot overflow on huge timestamps.
    let mid = iv.start_ts + (iv.end_ts - iv.start_ts) / 2;
    local_minute_of_day(tz, mid)
}

/// Splits a day's grid import and export by tariff band, classifying each
/// interval at its midpoint exactly as [`day_cost`] prices it, so the parts add
/// up to `day_cost`'s `import_kwh` and `export_kwh` for the same intervals.
pub fn grid_split(tariff: &Tariff, tz: Tz, intervals: &[Interval]) -> GridSplit {
    let mut split = GridSplit::default();
    for iv in intervals {
        let minute = interval_minute(tz, iv);
        match tariff.import_band_at(minute) {
            ImportBand::Free => split.import_free_kwh += iv.import_kwh,
            ImportBand::Shoulder => split.import_shoulder_kwh += iv.import_kwh,
            ImportBand::Peak => split.import_peak_kwh += iv.import_kwh,
        }
        match tariff.export_band_at(minute) {
            ExportBand::Super => split.export_super_kwh += iv.export_kwh,
            ExportBand::Peak => split.export_peak_kwh += iv.export_kwh,
            ExportBand::Other => split.export_other_kwh += iv.export_kwh,
        }
    }
    split
}

/// Local minutes since midnight for a unix time. We price an interval at its
/// midpoint, so an interval straddling a price change is not split.
pub(crate) fn local_minute_of_day(tz: Tz, ts: i64) -> u32 {
    // `from_timestamp` only fails for absurd times far outside real data;
    // minute 0 is a harmless fallback there.
    DateTime::from_timestamp(ts, 0).map_or(0, |utc| {
        let local = utc.with_timezone(&tz);
        local.hour() * 60 + local.minute()
    })
}

/// Cost one local day from its intervals. Each interval is priced at the local
/// time of its midpoint. An empty slice still pays the supply charge.
pub fn day_cost(tariff: &Tariff, tz: Tz, intervals: &[Interval]) -> DayCost {
    let mut cost = DayCost {
        import_kwh: 0.0,
        export_kwh: 0.0,
        load_kwh: 0.0,
        import_cents: 0.0,
        export_cents: 0.0,
        supply_cents: tariff.supply_cents_per_day,
        zero_import_credit_cents: 0.0,
        actual_cents: 0.0,
        baseline_cents: 0.0,
        savings_cents: 0.0,
    };
    // Import inside the credit window, and whether the window is finished in
    // the data: an interval inside it that ends in its last 15 minutes.
    // Without that we cannot claim the grid was unused, and a window still
    // under way (today) must not show the credit yet.
    let mut window_import_kwh = 0.0;
    let mut window_finished = false;
    let mut baseline_import_cents = 0.0;

    for iv in intervals {
        let minute = interval_minute(tz, iv);
        let import_price = tariff.import_cents_at(minute);

        cost.import_kwh += iv.import_kwh;
        cost.export_kwh += iv.export_kwh;
        cost.load_kwh += iv.load_kwh;
        cost.import_cents += iv.import_kwh * import_price;
        cost.export_cents += iv.export_kwh * tariff.export_cents_at(minute);
        baseline_import_cents += iv.load_kwh * import_price;

        if let Some(credit) = &tariff.zero_import_credit
            && credit.window.contains(minute)
        {
            window_import_kwh += iv.import_kwh;
            let end_minute = local_minute_of_day(tz, iv.end_ts);
            window_finished |= credit
                .window
                .ends_within(end_minute, WINDOW_FINISHED_MINUTES);
        }
    }

    if let Some(credit) = &tariff.zero_import_credit
        && window_finished
        && window_import_kwh <= credit.max_kwh
    {
        cost.zero_import_credit_cents = credit.credit_cents;
    }
    cost.actual_cents =
        cost.supply_cents + cost.import_cents - cost.export_cents - cost.zero_import_credit_cents;
    cost.baseline_cents = cost.supply_cents + baseline_import_cents;
    cost.savings_cents = cost.baseline_cents - cost.actual_cents;
    cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    /// Build a tariff from TOML, the same way the real config does.
    fn tariff(toml_text: &str) -> Tariff {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
        }
        let w: Wrapper = toml::from_str(toml_text).unwrap();
        Tariff::from_config(&w.tariff).unwrap()
    }

    /// The ZeroHero example from config.example.toml.
    fn zerohero() -> Tariff {
        tariff(
            r#"
            [tariff]
            supply_cents_per_day = 173.8
            default_import_cents = 44.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "15:00", to = "23:00", cents = 58.3 },
            ]
            default_export_cents = 0.0
            export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
            [tariff.zero_import_credit]
            from = "18:00"
            to = "21:00"
            max_kwh = 0.1
            credit_cents = 100.0
            "#,
        )
    }

    /// Unix time of a local Adelaide clock time on 2026-09-15 (no DST change
    /// that day, so +09:30).
    fn at(hour: u32, minute: u32) -> i64 {
        ADELAIDE
            .with_ymd_and_hms(2026, 9, 15, hour, minute, 0)
            .unwrap()
            .timestamp()
    }

    /// An interval from `from` to `to` (local times) with the given energy.
    fn iv(from: (u32, u32), to: (u32, u32), import: f64, export: f64, load: f64) -> Interval {
        Interval {
            start_ts: at(from.0, from.1),
            end_ts: at(to.0, to.1),
            import_kwh: import,
            export_kwh: export,
            load_kwh: load,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn the_key_changes_with_the_rates() {
        let base = "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 25.0\n";
        let dearer = base.replace("25.0", "26.0");
        assert_eq!(tariff(base).key(), tariff(base).key());
        assert_ne!(tariff(base).key(), tariff(&dearer).key());
    }

    #[test]
    fn default_rates_apply_outside_windows() {
        let t = zerohero();
        assert_eq!(t.import_cents_at(6 * 60), 44.0); // 06:00
        assert_eq!(t.import_cents_at(14 * 60 + 30), 44.0); // between the windows
        assert_eq!(t.export_cents_at(12 * 60), 0.0);
    }

    #[test]
    fn windows_include_start_and_exclude_end() {
        let t = zerohero();
        assert_eq!(t.import_cents_at(11 * 60), 0.0);
        assert_eq!(t.import_cents_at(14 * 60 - 1), 0.0);
        assert_eq!(t.import_cents_at(14 * 60), 44.0);
        assert_eq!(t.import_cents_at(15 * 60), 58.3);
        assert_eq!(t.import_cents_at(23 * 60 - 1), 58.3);
        assert_eq!(t.import_cents_at(23 * 60), 44.0);
    }

    #[test]
    fn window_can_wrap_past_midnight() {
        let t = tariff(
            r#"
            [tariff]
            supply_cents_per_day = 0.0
            default_import_cents = 30.0
            import = [{ from = "21:00", to = "06:00", cents = 10.0 }]
            "#,
        );
        assert_eq!(t.import_cents_at(20 * 60 + 59), 30.0);
        assert_eq!(t.import_cents_at(21 * 60), 10.0);
        assert_eq!(t.import_cents_at(0), 10.0); // midnight
        assert_eq!(t.import_cents_at(5 * 60 + 59), 10.0);
        assert_eq!(t.import_cents_at(6 * 60), 30.0);
    }

    #[test]
    fn first_matching_window_wins() {
        let t = tariff(
            r#"
            [tariff]
            supply_cents_per_day = 0.0
            default_import_cents = 30.0
            import = [
              { from = "10:00", to = "12:00", cents = 1.0 },
              { from = "09:00", to = "13:00", cents = 2.0 },
            ]
            "#,
        );
        assert_eq!(t.import_cents_at(9 * 60), 2.0); // only the second matches
        assert_eq!(t.import_cents_at(11 * 60), 1.0); // both match: first wins
    }

    #[test]
    fn intervals_are_priced_at_their_midpoint() {
        let t = zerohero();
        // 10:00-12:30 has its midpoint at 11:15 (free), though it starts at 44c.
        let early = day_cost(&t, ADELAIDE, &[iv((10, 0), (12, 30), 1.0, 0.0, 0.0)]);
        assert_eq!(early.import_cents, 0.0);
        // 12:30-15:30 has its midpoint at 14:00 exactly, which is outside the
        // free window (end-exclusive), so it costs the default 44c.
        let late = day_cost(&t, ADELAIDE, &[iv((12, 30), (15, 30), 1.0, 0.0, 0.0)]);
        assert!(close(late.import_cents, 44.0));
    }

    #[test]
    fn credit_is_earned_when_import_in_window_is_low() {
        let t = zerohero();
        let day = day_cost(
            &t,
            ADELAIDE,
            &[
                iv((18, 0), (19, 0), 0.05, 0.0, 1.0),
                iv((19, 0), (20, 0), 0.05, 0.0, 1.0), // total 0.1: still allowed
                iv((20, 0), (21, 0), 0.0, 0.0, 1.0),
                iv((22, 0), (23, 0), 5.0, 0.0, 5.0), // outside the window
            ],
        );
        assert_eq!(day.zero_import_credit_cents, 100.0);
    }

    #[test]
    fn credit_is_not_earned_when_import_in_window_is_high() {
        let t = zerohero();
        let day = day_cost(
            &t,
            ADELAIDE,
            &[
                iv((18, 0), (19, 0), 0.06, 0.0, 1.0),
                iv((19, 0), (20, 0), 0.06, 0.0, 1.0),
                iv((20, 0), (21, 0), 0.0, 0.0, 1.0),
            ],
        );
        assert_eq!(day.zero_import_credit_cents, 0.0);
    }

    #[test]
    fn credit_waits_for_the_window_to_finish() {
        let t = zerohero();
        // Today at 18:30: no import so far, but the window is not over.
        let partial = day_cost(&t, ADELAIDE, &[iv((18, 0), (18, 30), 0.0, 0.0, 1.0)]);
        assert_eq!(partial.zero_import_credit_cents, 0.0);
        // Data up to 20:30 is still short of the last 15 minutes.
        let nearly = day_cost(&t, ADELAIDE, &[iv((18, 0), (20, 30), 0.0, 0.0, 1.0)]);
        assert_eq!(nearly.zero_import_credit_cents, 0.0);
        // Data through to 21:00 earns it.
        let full = day_cost(&t, ADELAIDE, &[iv((18, 0), (21, 0), 0.0, 0.0, 1.0)]);
        assert_eq!(full.zero_import_credit_cents, 100.0);
        // A small gap at the end on a past day: the last data ends at 20:50.
        let gap = day_cost(&t, ADELAIDE, &[iv((18, 0), (20, 50), 0.0, 0.0, 1.0)]);
        assert_eq!(gap.zero_import_credit_cents, 100.0);
    }

    #[test]
    fn window_end_check_handles_midnight() {
        // 21:00 to 06:00 wraps; its last 15 minutes are 05:45 to 06:00.
        let w = Window {
            start: 21 * 60,
            end: 6 * 60,
        };
        assert!(w.ends_within(6 * 60, 15));
        assert!(w.ends_within(5 * 60 + 45, 15));
        assert!(!w.ends_within(5 * 60 + 44, 15));
        assert!(!w.ends_within(6 * 60 + 1, 15));
        // A window ending at midnight (end 0).
        let late = Window {
            start: 22 * 60,
            end: 0,
        };
        assert!(late.ends_within(0, 15));
        assert!(late.ends_within(23 * 60 + 50, 15));
        assert!(!late.ends_within(23 * 60, 15));
    }

    #[test]
    fn credit_needs_data_inside_the_window() {
        let t = zerohero();
        // Data only at lunchtime: we cannot say the evening used no grid power.
        let day = day_cost(&t, ADELAIDE, &[iv((12, 0), (13, 0), 0.0, 1.0, 1.0)]);
        assert_eq!(day.zero_import_credit_cents, 0.0);
        let empty = day_cost(&t, ADELAIDE, &[]);
        assert_eq!(empty.zero_import_credit_cents, 0.0);
        // An empty day still pays supply.
        assert_eq!(empty.actual_cents, 173.8);
        assert_eq!(empty.savings_cents, 0.0);
    }

    #[test]
    fn small_day_by_hand() {
        let t = zerohero();
        let day = day_cost(
            &t,
            ADELAIDE,
            &[
                // 06:00-07:00 midpoint 06:30, default 44c: buy 2 kWh, load 2 kWh.
                iv((6, 0), (7, 0), 2.0, 0.0, 2.0),
                // 12:00-13:00 midpoint 12:30, free import, 0c export: sell
                // 3 kWh, load 1 kWh (the rest of the sun went to the grid).
                iv((12, 0), (13, 0), 0.0, 3.0, 1.0),
                // 18:00-19:00 midpoint 18:30, 10c export: sell 1 kWh,
                // load 2 kWh from the battery, no import.
                iv((18, 0), (19, 0), 0.0, 1.0, 2.0),
                // 19:00-20:00 midpoint 19:30, 10c export, no import, load 1.
                iv((19, 0), (20, 0), 0.0, 1.0, 1.0),
                iv((20, 0), (21, 0), 0.0, 0.0, 0.0),
                // 21:00-22:00 midpoint 21:30, 58.3c: buy 1 kWh, load 1 kWh.
                iv((21, 0), (22, 0), 1.0, 0.0, 1.0),
            ],
        );
        assert!(close(day.import_kwh, 3.0));
        assert!(close(day.export_kwh, 5.0));
        assert!(close(day.load_kwh, 7.0));
        // Import: 2 * 44 + 1 * 58.3 = 146.3
        assert!(close(day.import_cents, 146.3));
        // Export: 3 * 0 + 1 * 10 + 1 * 10 = 20
        assert!(close(day.export_cents, 20.0));
        assert!(close(day.supply_cents, 173.8));
        // No import 18:00-21:00, so the credit is earned.
        assert!(close(day.zero_import_credit_cents, 100.0));
        // Actual: 173.8 + 146.3 - 20 - 100 = 200.1
        assert!(close(day.actual_cents, 200.1));
        // Baseline prices the load at import rates (06:30 is 44c, 12:30 is
        // free, the evening is 58.3c):
        // 2*44 + 1*0 + 2*58.3 + 1*58.3 + 1*58.3 = 321.2, plus supply 173.8.
        assert!(close(day.baseline_cents, 495.0));
        assert!(close(day.savings_cents, 495.0 - 200.1));
    }

    /// A schedule with plans importing at 10 c (to 2025-06-30), 20 c (to
    /// 2026-01-31) and then 30 c.
    fn schedule() -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
            previous_tariff: Vec<config::PreviousTariff>,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 1.0
            default_import_cents = 30.0
            [[previous_tariff]]
            until = "2025-06-30"
            supply_cents_per_day = 1.0
            default_import_cents = 10.0
            [[previous_tariff]]
            until = "2026-01-31"
            supply_cents_per_day = 1.0
            default_import_cents = 20.0
            "#,
        )
        .unwrap();
        TariffSchedule::from_config(&w.tariff, &w.previous_tariff).unwrap()
    }

    #[test]
    fn the_schedule_picks_the_plan_in_effect_on_each_day() {
        let schedule = schedule();
        let rate = |day: &str| schedule.for_date(day.parse().unwrap()).import_cents_at(0);
        assert_eq!(rate("2020-01-01"), 10.0);
        // The `until` day is still on its plan; the next day moves on.
        assert_eq!(rate("2025-06-30"), 10.0);
        assert_eq!(rate("2025-07-01"), 20.0);
        assert_eq!(rate("2026-01-31"), 20.0);
        assert_eq!(rate("2026-02-01"), 30.0);
        assert_eq!(rate("2030-01-01"), 30.0);
        // Without previous plans the current one covers every day.
        let single = TariffSchedule::single(tariff(
            "[tariff]\nsupply_cents_per_day = 1.0\ndefault_import_cents = 5.0\n",
        ));
        assert_eq!(
            single
                .for_date("2000-01-01".parse().unwrap())
                .import_cents_at(0),
            5.0
        );
    }

    /// The GloBird plan from config.example.toml.
    fn globird() -> Tariff {
        tariff(
            r#"
            [tariff]
            supply_cents_per_day = 173.8
            default_import_cents = 44.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "16:00", to = "23:00", cents = 58.3 },
            ]
            default_export_cents = 0.0
            export = [
              { from = "18:00", to = "21:00", cents = 10.0 },
              { from = "16:00", to = "23:00", cents = 2.0 },
            ]
            "#,
        )
    }

    /// 1 kWh each way at an instant (the midpoint is the instant itself).
    fn split_at(tariff: &Tariff, hour: u32, minute: u32) -> GridSplit {
        let t = at(hour, minute);
        let interval = Interval {
            start_ts: t,
            end_ts: t,
            import_kwh: 1.0,
            export_kwh: 1.0,
            load_kwh: 0.0,
        };
        grid_split(tariff, ADELAIDE, &[interval])
    }

    /// The (import, export) bands, as the index of the one field holding the kWh.
    fn bands(split: &GridSplit) -> (usize, usize) {
        let imports = [
            split.import_free_kwh,
            split.import_shoulder_kwh,
            split.import_peak_kwh,
        ];
        let exports = [
            split.export_super_kwh,
            split.export_peak_kwh,
            split.export_other_kwh,
        ];
        let at = |v: [f64; 3]| v.iter().position(|&x| x == 1.0).unwrap();
        (at(imports), at(exports))
    }

    #[test]
    fn the_split_follows_the_window_boundaries() {
        // (time, import band: 0 free 1 shoulder 2 peak, export band: 0 super 1 peak 2 other)
        let cases = [
            ((10, 59), (1, 2)),
            ((11, 0), (0, 2)),
            ((13, 59), (0, 2)),
            ((14, 0), (1, 2)),
            ((15, 59), (1, 2)),
            ((16, 0), (2, 1)),
            ((17, 59), (2, 1)),
            ((18, 0), (2, 0)),
            ((20, 59), (2, 0)),
            ((21, 0), (2, 1)),
            ((22, 59), (2, 1)),
            ((23, 0), (1, 2)),
        ];
        for ((h, m), want) in cases {
            let split = split_at(&globird(), h, m);
            assert_eq!(bands(&split), want, "at {h}:{m:02}");
        }
    }

    #[test]
    fn the_split_adds_up_to_the_day_cost_energy() {
        let tariff = globird();
        let intervals = [
            iv((9, 0), (11, 30), 1.5, 0.2, 2.0),
            iv((12, 0), (13, 0), 2.0, 0.0, 2.0),
            iv((17, 0), (18, 30), 0.7, 0.9, 1.0),
            iv((19, 0), (20, 0), 0.0, 3.0, 0.5),
            iv((21, 30), (22, 0), 0.4, 0.1, 0.4),
        ];
        let cost = day_cost(&tariff, ADELAIDE, &intervals);
        let split = grid_split(&tariff, ADELAIDE, &intervals);
        let imports = split.import_free_kwh + split.import_shoulder_kwh + split.import_peak_kwh;
        let exports = split.export_super_kwh + split.export_peak_kwh + split.export_other_kwh;
        assert!(close(imports, cost.import_kwh));
        assert!(close(exports, cost.export_kwh));
        assert!(close(split.import_free_kwh, 2.0));
    }

    #[test]
    fn a_plan_without_windows_has_only_shoulder_and_other() {
        let flat = tariff(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 30.0\ndefault_export_cents = 5.0\n",
        );
        assert_eq!(bands(&split_at(&flat, 12, 0)), (1, 2));
        assert_eq!(bands(&split_at(&flat, 19, 0)), (1, 2));
    }
}
