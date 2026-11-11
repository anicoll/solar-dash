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

#[derive(Debug, Clone, Copy)]
struct ExportTier {
    first_kwh: f64,
    cents: f64,
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
    export_tier: Option<ExportTier>,
    dynamic: Option<Dynamic>,
    /// Identifies these exact rates; see [`Tariff::key`].
    key: String,
}

/// The settings of a plan priced from stored live prices.
#[derive(Debug, Clone)]
struct Dynamic {
    source: String,
    fee_cents_per_day: f64,
    feed_in_paid_when_negative: bool,
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
        let dynamic = cfg
            .dynamic
            .as_ref()
            .map(|d| {
                anyhow::ensure!(
                    !d.source.trim().is_empty(),
                    "source in the dynamic prices must not be empty"
                );
                anyhow::ensure!(
                    d.fee_cents_per_day.is_finite(),
                    "fee_cents_per_day in the dynamic prices must be a number"
                );
                Ok::<_, anyhow::Error>(Dynamic {
                    source: d.source.trim().to_string(),
                    fee_cents_per_day: d.fee_cents_per_day,
                    feed_in_paid_when_negative: d.feed_in_paid_when_negative,
                })
            })
            .transpose()
            .context("invalid dynamic prices in [tariff]")?;
        Ok(Self {
            supply_cents_per_day: cfg.supply_cents_per_day,
            default_import_cents: cfg.default_import_cents,
            import: parse_priced(&cfg.import).context("invalid import window in [tariff]")?,
            default_export_cents: cfg.default_export_cents,
            export: parse_priced(&cfg.export).context("invalid export window in [tariff]")?,
            zero_import_credit,
            export_tier: cfg.export_tier.as_ref().map(|t| ExportTier {
                first_kwh: t.first_kwh_per_day,
                cents: t.cents,
            }),
            dynamic,
            key: serde_json::to_string(cfg).context("could not describe the [tariff] section")?,
        })
    }

    /// A string that is the same for the same rates and differs when any rate
    /// or window changes (it is the config section written as JSON). Stored
    /// costs are tagged with it, so editing config.toml makes them stale.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The price source prefix when this plan is priced from stored live
    /// prices; `None` for an ordinary (static) plan.
    pub fn dynamic_source(&self) -> Option<&str> {
        self.dynamic.as_ref().map(|d| d.source.as_str())
    }

    /// The fixed charge for one day: the supply charge plus a dynamic plan's
    /// retailer fee (nothing is added for a static plan).
    fn daily_supply_cents(&self) -> f64 {
        self.supply_cents_per_day + self.dynamic.as_ref().map_or(0.0, |d| d.fee_cents_per_day)
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

    /// The daily feed-in tier as `(first_kwh_per_day, cents)`, or `None`.
    pub fn export_tier(&self) -> Option<(f64, f64)> {
        self.export_tier.map(|t| (t.first_kwh, t.cents))
    }

    /// Splits `kwh` exported now, after `already_kwh` earlier the same local
    /// day, into `(tier_kwh, rest_kwh)`: the part still inside the daily tier
    /// (paid the tier rate) and the part paid by time of day. Without a tier
    /// everything is `rest`.
    fn split_export_tier(&self, already_kwh: f64, kwh: f64) -> (f64, f64) {
        let Some(tier) = self.export_tier else {
            return (0.0, kwh);
        };
        let tier_kwh = (tier.first_kwh - already_kwh).max(0.0).min(kwh);
        (tier_kwh, kwh - tier_kwh)
    }

    /// Cents earned for exporting `kwh` at this local time, after
    /// `already_kwh` earlier the same local day (for the daily export tier).
    pub fn export_value_cents(&self, minute_of_day: u32, already_kwh: f64, kwh: f64) -> f64 {
        self.export_value_at_rate(already_kwh, kwh, self.export_cents_at(minute_of_day))
    }

    /// Like [`Tariff::export_value_cents`], but the part outside the daily
    /// tier is paid `rest_cents` instead of the time-of-day rate. A dynamic
    /// plan passes the live feed-in price here, so the tier still applies.
    fn export_value_at_rate(&self, already_kwh: f64, kwh: f64, rest_cents: f64) -> f64 {
        let (tier_kwh, rest_kwh) = self.split_export_tier(already_kwh, kwh);
        let tier_cents = self.export_tier.map_or(0.0, |t| t.cents);
        tier_kwh * tier_cents + rest_kwh * rest_cents
    }

    /// The import price averaged over the 24 hours of a day, in cents per kWh.
    pub fn average_import_cents(&self) -> f64 {
        (0..24 * 60).map(|m| self.import_cents_at(m)).sum::<f64>() / (24.0 * 60.0)
    }

    /// The highest import price at any time of day, in cents per kWh.
    pub fn peak_import_cents(&self) -> f64 {
        (0..24 * 60)
            .map(|m| self.import_cents_at(m))
            .fold(f64::MIN, f64::max)
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
    /// The configured plan names: one per previous plan, then the current one.
    names: Vec<Option<String>>,
}

/// One plan of a [`TariffSchedule`], for labelling.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSpan {
    /// The `name` from the config, if it has one.
    pub name: Option<String>,
    /// The first local day it applied; `None` for the first plan.
    pub from: Option<NaiveDate>,
    /// The last local day it applied; `None` for the current plan.
    pub until: Option<NaiveDate>,
}

impl TariffSchedule {
    /// A schedule with one plan for every day.
    pub fn single(current: Tariff) -> Self {
        Self {
            previous: Vec::new(),
            current,
            names: vec![None],
        }
    }

    /// Convert the config. `Config::load` already checks the dates and order,
    /// so an error here means the config was built some other way.
    pub fn from_config(
        current: &config::Tariff,
        previous_cfg: &[config::PreviousTariff],
    ) -> Result<Self> {
        let previous = previous_cfg
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
        let names = previous_cfg
            .iter()
            .map(|p| p.name.clone())
            .chain([current.name.clone()])
            .collect();
        Ok(Self {
            previous,
            current: Tariff::from_config(current)?,
            names,
        })
    }

    /// Builds the schedule from plans kept as data: each plan with the first
    /// local day it applies (`None` for a plan with no known start, allowed
    /// only for the earliest). A plan lasts until the day before the next
    /// one starts. Gives the same schedule as [`TariffSchedule::from_config`]
    /// for the same plans, including each plan's `key`.
    pub fn from_plans(plans: &[(Option<NaiveDate>, config::Tariff)]) -> Result<Self> {
        anyhow::ensure!(!plans.is_empty(), "there are no tariff plans");
        let mut sorted: Vec<&(Option<NaiveDate>, config::Tariff)> = plans.iter().collect();
        // `None` sorts before every date, so a plan with no start comes first.
        sorted.sort_by_key(|(from, _)| *from);
        anyhow::ensure!(
            sorted.iter().skip(1).all(|(from, _)| from.is_some()),
            "only the earliest plan may have no start date"
        );
        anyhow::ensure!(
            sorted.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "two plans start on the same day"
        );
        let (last, earlier) = sorted.split_last().expect("checked not empty");
        let mut previous = Vec::new();
        for (i, (_, tariff)) in earlier.iter().enumerate() {
            // The next plan has a start date: only the first can lack one.
            let next_start = sorted[i + 1].0.context("a later plan has no start date")?;
            let until = next_start
                .pred_opt()
                .context("plan start date is too early")?;
            let tariff = Tariff::from_config(tariff)
                .with_context(|| format!("invalid tariff plan #{}", i + 1))?;
            previous.push((until, tariff));
        }
        let names = sorted.iter().map(|(_, t)| t.name.clone()).collect();
        Ok(Self {
            previous,
            current: Tariff::from_config(&last.1)?,
            names,
        })
    }

    /// Which plan applies on this local day: 0 for the oldest, up to the
    /// number of previous plans for the current one. The same choice as
    /// [`TariffSchedule::for_date`].
    pub fn plan_index(&self, date: NaiveDate) -> usize {
        self.previous
            .iter()
            .position(|(until, _)| *until >= date)
            .unwrap_or(self.previous.len())
    }

    /// Every plan, oldest first, with the days it applied.
    pub fn plan_spans(&self) -> Vec<PlanSpan> {
        let mut from = None;
        let mut spans = Vec::new();
        for (i, name) in self.names.iter().enumerate() {
            let until = self.previous.get(i).map(|(until, _)| *until);
            spans.push(PlanSpan {
                name: name.clone(),
                from,
                until,
            });
            from = until.and_then(|u| u.succ_opt());
        }
        spans
    }

    /// Each plan, oldest first, as the first local day it applied and its
    /// name. The first plan has no known start (`None`); every later one
    /// starts the day after the one before ended.
    pub fn plans(&self) -> Vec<(Option<NaiveDate>, Option<&str>)> {
        let starts = std::iter::once(None)
            .chain(self.previous.iter().map(|(until, _)| until.succ_opt()))
            .take(self.names.len());
        starts
            .zip(&self.names)
            .map(|(start, name)| (start, name.as_deref()))
            .collect()
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
    /// Export paid the daily tier rate (`export_tier`), whatever the time of
    /// day. The other `export_*` parts hold only the exports after the tier.
    pub export_tier_kwh: f64,
}

/// The intervals in time order, which the daily export tier needs (it counts
/// kWh from midnight). Only copies when a tier exists and they are out of order.
fn in_time_order<'a>(
    tariff: &Tariff,
    intervals: &'a [Interval],
) -> std::borrow::Cow<'a, [Interval]> {
    let sorted = intervals.windows(2).all(|p| p[0].start_ts <= p[1].start_ts);
    if tariff.export_tier.is_none() || sorted {
        return std::borrow::Cow::Borrowed(intervals);
    }
    let mut copy = intervals.to_vec();
    copy.sort_by_key(|iv| iv.start_ts);
    std::borrow::Cow::Owned(copy)
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
/// Export inside the daily `export_tier` is counted in `export_tier_kwh`
/// (splitting an interval that crosses the threshold); only the rest is
/// classified by time of day.
#[cfg(test)]
pub fn grid_split(tariff: &Tariff, tz: Tz, intervals: &[Interval]) -> GridSplit {
    grid_split_priced(tariff, tz, intervals, None, 0.0)
}

/// Like [`grid_split`], but with stored `prices` an interval whose price is
/// known is banded by that price: import by [`live_import_band`] against
/// `spike_cents`, feed-in as `Other` when it earns nothing or less, `Super`
/// above `spike_cents`, otherwise `Peak`. Intervals without a price row use
/// the plan's windows, as [`day_cost_priced`] does. With `None` this is
/// exactly [`grid_split`].
pub fn grid_split_priced(
    tariff: &Tariff,
    tz: Tz,
    intervals: &[Interval],
    prices: Option<&DayPrices>,
    spike_cents: f64,
) -> GridSplit {
    let mut split = GridSplit::default();
    let mut exported = 0.0;
    for iv in in_time_order(tariff, intervals).iter() {
        let minute = interval_minute(tz, iv);
        let mid = iv.start_ts + (iv.end_ts - iv.start_ts) / 2;
        let import_band = match prices.and_then(|p| span_at(&p.general, mid)) {
            Some(cents) => live_import_band(cents, spike_cents),
            None => tariff.import_band_at(minute),
        };
        match import_band {
            ImportBand::Free => split.import_free_kwh += iv.import_kwh,
            ImportBand::Shoulder => split.import_shoulder_kwh += iv.import_kwh,
            ImportBand::Peak => split.import_peak_kwh += iv.import_kwh,
        }
        let (tier_kwh, rest_kwh) = tariff.split_export_tier(exported, iv.export_kwh);
        exported += iv.export_kwh;
        split.export_tier_kwh += tier_kwh;
        let export_band = match prices.and_then(|p| span_at(&p.feed_in, mid)) {
            Some(stored) => {
                let earning = tariff.feed_in_earning(stored);
                if earning <= 0.0 {
                    ExportBand::Other
                } else if earning > spike_cents {
                    ExportBand::Super
                } else {
                    ExportBand::Peak
                }
            }
            None => tariff.export_band_at(minute),
        };
        match export_band {
            ExportBand::Super => split.export_super_kwh += rest_kwh,
            ExportBand::Peak => split.export_peak_kwh += rest_kwh,
            ExportBand::Other => split.export_other_kwh += rest_kwh,
        }
    }
    split
}

/// One stored price: c/kWh including GST over `[start_ts, end_ts)`, as the
/// source gave it (a feed-in price has not had its sign changed).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceSpan {
    pub start_ts: i64,
    pub end_ts: i64,
    pub cents: f64,
}

/// The stored prices for one local day: import (`general`) and `feed_in`
/// spans, sorted by start. The caller decides which rows go in: finished
/// `actual` prices, plus `current` for today, and never `forecast`.
#[derive(Debug, Clone, Default)]
pub struct DayPrices {
    pub(crate) general: Vec<PriceSpan>,
    pub(crate) feed_in: Vec<PriceSpan>,
}

/// The span covering `ts`, if any. `spans` must be sorted by start.
fn span_at(spans: &[PriceSpan], ts: i64) -> Option<f64> {
    // The last span that starts at or before `ts`; it covers `ts` only if it
    // has not ended yet.
    let next = spans.partition_point(|s| s.start_ts <= ts);
    let span = spans.get(next.checked_sub(1)?)?;
    (ts < span.end_ts).then_some(span.cents)
}

impl DayPrices {
    pub fn new(mut general: Vec<PriceSpan>, mut feed_in: Vec<PriceSpan>) -> Self {
        // Two sources under one prefix can price the same start; keep one.
        for spans in [&mut general, &mut feed_in] {
            spans.sort_by_key(|s| s.start_ts);
            spans.dedup_by_key(|s| s.start_ts);
        }
        Self { general, feed_in }
    }
}

/// A day's cost from stored prices, and how much of it had to fall back to
/// the plan's own windows because no price row covered the time.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedDayCost {
    pub cost: DayCost,
    /// Minutes of energy intervals priced from the plan's windows, because
    /// the import or the feed-in price was missing. 0 means every interval
    /// was priced from stored prices.
    pub fallback_minutes: u32,
}

/// The band of an import price when prices come from a live source: nothing
/// to pay is `Free`, above `spike_cents` is `Peak`, anything else `Shoulder`.
/// The same rule the Today page uses to colour the live price.
pub fn live_import_band(cents: f64, spike_cents: f64) -> ImportBand {
    if cents <= 0.0 {
        ImportBand::Free
    } else if cents > spike_cents {
        ImportBand::Peak
    } else {
        ImportBand::Shoulder
    }
}

impl Tariff {
    /// What a stored feed-in price earns, in cents per kWh (positive means
    /// money in). Follows the plan's `feed_in_paid_when_negative` setting.
    fn feed_in_earning(&self, stored_cents: f64) -> f64 {
        match &self.dynamic {
            Some(d) if !d.feed_in_paid_when_negative => stored_cents,
            // Flip the sign. Written as a subtraction so 0 stays 0, not -0.
            _ => 0.0 - stored_cents,
        }
    }
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
/// The app itself always calls [`day_cost_with_window_import`]; this shorter
/// form keeps the tests readable.
#[cfg(test)]
pub fn day_cost(tariff: &Tariff, tz: Tz, intervals: &[Interval]) -> DayCost {
    day_cost_with_window_import(tariff, tz, intervals, None)
}

/// Like [`day_cost`], but the zero-import credit is decided from
/// `window_import_override_kwh` (the grid import inside the credit window as
/// the meter counter saw it) instead of the sum of the intervals. Money still
/// comes from the intervals. The window must still be finished in the data.
pub fn day_cost_with_window_import(
    tariff: &Tariff,
    tz: Tz,
    intervals: &[Interval],
    window_import_override_kwh: Option<f64>,
) -> DayCost {
    day_cost_priced(tariff, tz, intervals, None, window_import_override_kwh).cost
}

/// The one costing loop. With `prices` of `None` every interval is priced
/// from the plan's windows, exactly as a static plan always was. With prices,
/// an interval takes the stored general price (import) and feed-in price
/// (export) that cover its midpoint, and falls back to the windows for
/// whichever one is missing. The credit, the export tier and the supply
/// charge come from the plan either way.
pub fn day_cost_priced(
    tariff: &Tariff,
    tz: Tz,
    intervals: &[Interval],
    prices: Option<&DayPrices>,
    window_import_override_kwh: Option<f64>,
) -> PricedDayCost {
    let mut cost = DayCost {
        import_kwh: 0.0,
        export_kwh: 0.0,
        load_kwh: 0.0,
        import_cents: 0.0,
        export_cents: 0.0,
        supply_cents: tariff.daily_supply_cents(),
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
    // Export so far today, for the daily export tier.
    let mut exported = 0.0;
    let mut fallback_secs = 0_i64;

    for iv in in_time_order(tariff, intervals).iter() {
        let minute = interval_minute(tz, iv);
        let mid = iv.start_ts + (iv.end_ts - iv.start_ts) / 2;
        let live_import = prices.and_then(|p| span_at(&p.general, mid));
        let live_export = prices.and_then(|p| span_at(&p.feed_in, mid));
        if prices.is_some() && (live_import.is_none() || live_export.is_none()) {
            fallback_secs += (iv.end_ts - iv.start_ts).max(0);
        }
        let import_price = live_import.unwrap_or_else(|| tariff.import_cents_at(minute));

        cost.import_kwh += iv.import_kwh;
        cost.export_kwh += iv.export_kwh;
        cost.load_kwh += iv.load_kwh;
        cost.import_cents += iv.import_kwh * import_price;
        cost.export_cents += match live_export {
            Some(stored) => {
                tariff.export_value_at_rate(exported, iv.export_kwh, tariff.feed_in_earning(stored))
            }
            None => tariff.export_value_cents(minute, exported, iv.export_kwh),
        };
        exported += iv.export_kwh;
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
        && window_import_override_kwh.unwrap_or(window_import_kwh) <= credit.max_kwh
    {
        cost.zero_import_credit_cents = credit.credit_cents;
    }
    cost.actual_cents =
        cost.supply_cents + cost.import_cents - cost.export_cents - cost.zero_import_credit_cents;
    cost.baseline_cents = cost.supply_cents + baseline_import_cents;
    cost.savings_cents = cost.baseline_cents - cost.actual_cents;
    PricedDayCost {
        cost,
        fallback_minutes: (fallback_secs / 60) as u32,
    }
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
            supply_cents_per_day = 150.0
            default_import_cents = 40.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "15:00", to = "23:00", cents = 60.0 },
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
    fn a_window_import_override_decides_the_credit() {
        let t = zerohero();
        // The intervals saw 0.15 kWh in the window; the meter says 0.1.
        let day = [iv((18, 0), (21, 0), 0.15, 0.0, 1.0)];
        assert_eq!(day_cost(&t, ADELAIDE, &day).zero_import_credit_cents, 0.0);
        let earned = day_cost_with_window_import(&t, ADELAIDE, &day, Some(0.1));
        assert_eq!(earned.zero_import_credit_cents, 100.0);
        // Money is still worked out from the intervals.
        assert!(close(earned.import_kwh, 0.15));
        // And the other way round.
        let quiet = [iv((18, 0), (21, 0), 0.0, 0.0, 1.0)];
        let missed = day_cost_with_window_import(&t, ADELAIDE, &quiet, Some(0.2));
        assert_eq!(missed.zero_import_credit_cents, 0.0);
        // An unfinished window still never earns it.
        let early = [iv((18, 0), (19, 0), 0.0, 0.0, 1.0)];
        let early = day_cost_with_window_import(&t, ADELAIDE, &early, Some(0.0));
        assert_eq!(early.zero_import_credit_cents, 0.0);
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
        assert_eq!(t.import_cents_at(6 * 60), 40.0); // 06:00
        assert_eq!(t.import_cents_at(14 * 60 + 30), 40.0); // between the windows
        assert_eq!(t.export_cents_at(12 * 60), 0.0);
    }

    #[test]
    fn windows_include_start_and_exclude_end() {
        let t = zerohero();
        assert_eq!(t.import_cents_at(11 * 60), 0.0);
        assert_eq!(t.import_cents_at(14 * 60 - 1), 0.0);
        assert_eq!(t.import_cents_at(14 * 60), 40.0);
        assert_eq!(t.import_cents_at(15 * 60), 60.0);
        assert_eq!(t.import_cents_at(23 * 60 - 1), 60.0);
        assert_eq!(t.import_cents_at(23 * 60), 40.0);
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
    fn peak_import_is_the_dearest_price_of_the_day() {
        assert_eq!(zerohero().peak_import_cents(), 60.0);
        // A window shadowed by an earlier one does not count.
        let t = tariff(
            r#"
            [tariff]
            supply_cents_per_day = 0.0
            default_import_cents = 30.0
            import = [
              { from = "09:00", to = "13:00", cents = 1.0 },
              { from = "10:00", to = "12:00", cents = 99.0 },
            ]
            "#,
        );
        assert_eq!(t.peak_import_cents(), 30.0);
    }

    #[test]
    fn plans_list_their_start_days_and_names() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
            previous_tariff: Vec<config::PreviousTariff>,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            name = "Now"
            supply_cents_per_day = 100.0
            default_import_cents = 30.0
            [[previous_tariff]]
            name = "Old"
            until = "2024-06-30"
            supply_cents_per_day = 100.0
            default_import_cents = 20.0
            [[previous_tariff]]
            until = "2025-06-30"
            supply_cents_per_day = 100.0
            default_import_cents = 25.0
            "#,
        )
        .unwrap();
        let schedule = TariffSchedule::from_config(&w.tariff, &w.previous_tariff).unwrap();
        let day = |text: &str| text.parse::<NaiveDate>().ok();
        assert_eq!(
            schedule.plans(),
            vec![
                (None, Some("Old")),
                (day("2024-07-01"), None),
                (day("2025-07-01"), Some("Now")),
            ]
        );
        let single = TariffSchedule::single(Tariff::from_config(&w.tariff).unwrap());
        assert_eq!(single.plans(), vec![(None, None)]);
    }

    /// Three plans written as config, to compare `from_plans` with `from_config`.
    fn three_plan_config() -> (config::Tariff, Vec<config::PreviousTariff>) {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
            previous_tariff: Vec<config::PreviousTariff>,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            name = "Now"
            supply_cents_per_day = 100.0
            default_import_cents = 30.0
            [[previous_tariff]]
            name = "Old"
            until = "2024-06-30"
            supply_cents_per_day = 100.0
            default_import_cents = 20.0
            [[previous_tariff]]
            until = "2025-06-30"
            supply_cents_per_day = 100.0
            default_import_cents = 25.0
            "#,
        )
        .unwrap();
        (w.tariff, w.previous_tariff)
    }

    #[test]
    fn from_plans_builds_the_same_schedule_as_from_config() {
        let (current, previous) = three_plan_config();
        let day = |text: &str| text.parse::<NaiveDate>().unwrap();
        let named = |plan: &config::PreviousTariff| config::Tariff {
            name: plan.name.clone(),
            ..plan.tariff.clone()
        };
        // Given out of order on purpose.
        let plans = [
            (Some(day("2025-07-01")), current.clone()),
            (None, named(&previous[0])),
            (Some(day("2024-07-01")), named(&previous[1])),
        ];
        let built = TariffSchedule::from_plans(&plans).unwrap();
        let expected = TariffSchedule::from_config(&current, &previous).unwrap();
        assert_eq!(built.plan_spans(), expected.plan_spans());
        for date in [
            "2020-01-01",
            "2024-06-30",
            "2024-07-01",
            "2025-06-30",
            "2025-07-01",
        ] {
            assert_eq!(
                built.for_date(day(date)).key(),
                expected.for_date(day(date)).key()
            );
        }
    }

    #[test]
    fn from_plans_rejects_bad_sets() {
        let (current, _) = three_plan_config();
        let day = |text: &str| text.parse::<NaiveDate>().unwrap();
        assert!(TariffSchedule::from_plans(&[]).is_err());
        // Only the earliest plan may have no start.
        let two_open = [(None, current.clone()), (None, current.clone())];
        assert!(TariffSchedule::from_plans(&two_open).is_err());
        let same_day = [
            (Some(day("2025-01-01")), current.clone()),
            (Some(day("2025-01-01")), current.clone()),
        ];
        assert!(TariffSchedule::from_plans(&same_day).is_err());
        let one = [(Some(day("2025-01-01")), current)];
        let schedule = TariffSchedule::from_plans(&one).unwrap();
        assert_eq!(schedule.plans().len(), 1);
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
        // 10:00-12:30 has its midpoint at 11:15 (free), though it starts at 40c.
        let early = day_cost(&t, ADELAIDE, &[iv((10, 0), (12, 30), 1.0, 0.0, 0.0)]);
        assert_eq!(early.import_cents, 0.0);
        // 12:30-15:30 has its midpoint at 14:00 exactly, which is outside the
        // free window (end-exclusive), so it costs the default 40c.
        let late = day_cost(&t, ADELAIDE, &[iv((12, 30), (15, 30), 1.0, 0.0, 0.0)]);
        assert!(close(late.import_cents, 40.0));
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
        assert_eq!(empty.actual_cents, 150.0);
        assert_eq!(empty.savings_cents, 0.0);
    }

    #[test]
    fn small_day_by_hand() {
        let t = zerohero();
        let day = day_cost(
            &t,
            ADELAIDE,
            &[
                // 06:00-07:00 midpoint 06:30, default 40c: buy 2 kWh, load 2 kWh.
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
                // 21:00-22:00 midpoint 21:30, 60.0c: buy 1 kWh, load 1 kWh.
                iv((21, 0), (22, 0), 1.0, 0.0, 1.0),
            ],
        );
        assert!(close(day.import_kwh, 3.0));
        assert!(close(day.export_kwh, 5.0));
        assert!(close(day.load_kwh, 7.0));
        // Import: 2 * 40 + 1 * 60 = 140
        assert!(close(day.import_cents, 140.0));
        // Export: 3 * 0 + 1 * 10 + 1 * 10 = 20
        assert!(close(day.export_cents, 20.0));
        assert!(close(day.supply_cents, 150.0));
        // No import 18:00-21:00, so the credit is earned.
        assert!(close(day.zero_import_credit_cents, 100.0));
        // Actual: 150 + 140 - 20 - 100 = 170
        assert!(close(day.actual_cents, 170.0));
        // Baseline prices the load at import rates (06:30 is 40c, 12:30 is
        // free, the evening is 60.0c):
        // 2*40 + 1*0 + 2*60.0 + 1*60.0 + 1*60.0 = 320, plus supply 150.0.
        assert!(close(day.baseline_cents, 470.0));
        assert!(close(day.savings_cents, 470.0 - 170.0));
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

    #[test]
    fn plans_are_listed_with_their_days() {
        let schedule = schedule();
        let day = |text: &str| -> NaiveDate { text.parse().unwrap() };
        let plans = schedule.plan_spans();
        assert_eq!(plans.len(), 3);
        assert_eq!(
            (plans[0].from, plans[0].until),
            (None, Some(day("2025-06-30")))
        );
        assert_eq!(
            (plans[1].from, plans[1].until),
            (Some(day("2025-07-01")), Some(day("2026-01-31")))
        );
        assert_eq!(
            (plans[2].from, plans[2].until),
            (Some(day("2026-02-01")), None)
        );
        // The index is the same choice as `for_date`.
        assert_eq!(schedule.plan_index(day("2025-06-30")), 0);
        assert_eq!(schedule.plan_index(day("2025-07-01")), 1);
        assert_eq!(schedule.plan_index(day("2026-02-01")), 2);
    }

    #[test]
    fn a_plan_name_does_not_change_the_stored_key() {
        let plain = tariff("[tariff]\nsupply_cents_per_day = 1.0\ndefault_import_cents = 5.0\n");
        let named = tariff(
            "[tariff]\nname = \"Flat\"\nsupply_cents_per_day = 1.0\ndefault_import_cents = 5.0\n",
        );
        assert_eq!(plain.key(), named.key());
    }

    /// The GloBird plan from config.example.toml.
    fn globird() -> Tariff {
        tariff(
            r#"
            [tariff]
            supply_cents_per_day = 150.0
            default_import_cents = 40.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "16:00", to = "23:00", cents = 60.0 },
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

    /// The one price of a band: the windows in it, else the default if it falls in the band.
    fn band_price(windows: &[(u32, u32, f64)], default: f64, in_band: impl Fn(f64) -> bool) -> f64 {
        windows
            .iter()
            .map(|w| w.2)
            .find(|&c| in_band(c))
            .unwrap_or(default)
    }

    #[test]
    fn the_by_tariff_table_arithmetic_reproduces_the_day_cost() {
        // What the History "By tariff" table does: band kWh times the band's rate,
        // plus supply, less credit, equals `actual_cents` while each band has one price.
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
        let imp = tariff.import_windows();
        let exp = tariff.export_windows();
        let (di, de) = (tariff.default_import_cents(), tariff.default_export_cents());
        let buy = |band| band_price(&imp, di, |c| tariff.import_band_for(c) == band);
        let sell = |band| band_price(&exp, de, |c| tariff.export_band_for(c) == band);
        let cents = split.import_free_kwh * buy(ImportBand::Free)
            + split.import_shoulder_kwh * di
            + split.import_peak_kwh * buy(ImportBand::Peak)
            - split.export_super_kwh * sell(ExportBand::Super)
            - split.export_peak_kwh * sell(ExportBand::Peak)
            - split.export_other_kwh * de
            + tariff.supply_cents_per_day()
            - cost.zero_import_credit_cents;
        assert!(
            close(cents, cost.actual_cents),
            "{cents} vs {}",
            cost.actual_cents
        );
    }

    #[test]
    fn a_plan_without_windows_has_only_shoulder_and_other() {
        let flat = tariff(
            "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 30.0\ndefault_export_cents = 5.0\n",
        );
        assert_eq!(bands(&split_at(&flat, 12, 0)), (1, 2));
        assert_eq!(bands(&split_at(&flat, 19, 0)), (1, 2));
    }

    /// The old plan: first 14 kWh a day at 15c, then 5c (with a 10c window).
    const TIERED: &str = r#"
        [tariff]
        supply_cents_per_day = 100.0
        default_import_cents = 30.0
        default_export_cents = 5.0
        export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
        [tariff.export_tier]
        first_kwh_per_day = 14.0
        cents = 15.0
    "#;

    #[test]
    fn the_tier_boundary_splits_one_interval() {
        let t = tariff(TIERED);
        // 13.9 kWh by midday, then 0.5 kWh: 0.1 in the tier, 0.4 after it.
        let intervals = [
            iv((10, 0), (10, 5), 0.0, 13.9, 0.0),
            iv((12, 0), (12, 5), 0.0, 0.5, 0.0),
        ];
        let cost = day_cost(&t, ADELAIDE, &intervals);
        assert!(close(cost.export_kwh, 14.4));
        assert!(close(cost.export_cents, 14.0 * 15.0 + 0.4 * 5.0));
        let split = grid_split(&t, ADELAIDE, &intervals);
        assert!(close(split.export_tier_kwh, 14.0));
        assert!(close(split.export_other_kwh, 0.4));
        assert!(close(split.export_super_kwh + split.export_peak_kwh, 0.0));
    }

    #[test]
    fn days_below_and_above_the_threshold() {
        let t = tariff(TIERED);
        let below = [iv((12, 0), (12, 5), 0.0, 6.0, 0.0)];
        let cost = day_cost(&t, ADELAIDE, &below);
        assert!(close(cost.export_cents, 6.0 * 15.0));
        let split = grid_split(&t, ADELAIDE, &below);
        assert!(close(split.export_tier_kwh, 6.0));
        assert!(close(split.export_other_kwh, 0.0));

        // 20 kWh: the first 14 in time order are at 15c wherever they fall,
        // so the 10c evening window is only paid after the tier is used up.
        let above = [
            iv((9, 0), (9, 5), 0.0, 10.0, 0.0),
            iv((17, 0), (17, 5), 0.0, 4.0, 0.0),
            iv((19, 0), (19, 5), 0.0, 6.0, 0.0),
        ];
        let cost = day_cost(&t, ADELAIDE, &above);
        assert!(close(cost.export_cents, 14.0 * 15.0 + 6.0 * 10.0));
        let split = grid_split(&t, ADELAIDE, &above);
        assert!(close(split.export_tier_kwh, 14.0));
        assert!(close(split.export_super_kwh + split.export_peak_kwh, 6.0));
        // Parts add up to the exported energy, and out-of-order input is sorted.
        let reversed: Vec<_> = above.iter().rev().copied().collect();
        assert!(close(
            day_cost(&t, ADELAIDE, &reversed).export_cents,
            cost.export_cents
        ));
        assert_eq!(grid_split(&t, ADELAIDE, &reversed), split);
    }

    #[test]
    fn plans_without_a_tier_are_unchanged() {
        let t = zerohero();
        assert!(t.export_tier().is_none());
        let cost = day_cost(&t, ADELAIDE, &[iv((19, 0), (19, 5), 0.0, 30.0, 0.0)]);
        assert!(close(cost.export_cents, 300.0));
        // The key does not mention the tier, so stored costs stay valid.
        assert!(!t.key().contains("export_tier"));
        assert!(tariff(TIERED).key().contains("export_tier"));
    }

    #[test]
    fn the_tier_on_a_previous_plan_only_applies_to_its_days() {
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
            default_export_cents = 5.0
            [[previous_tariff]]
            until = "2026-06-30"
            supply_cents_per_day = 1.0
            default_import_cents = 30.0
            default_export_cents = 5.0
            [previous_tariff.export_tier]
            first_kwh_per_day = 14.0
            cents = 15.0
            "#,
        )
        .unwrap();
        let schedule = TariffSchedule::from_config(&w.tariff, &w.previous_tariff).unwrap();
        let day = [iv((12, 0), (12, 5), 0.0, 20.0, 0.0)];
        let cost_on = |date: &str| {
            day_cost(schedule.for_date(date.parse().unwrap()), ADELAIDE, &day).export_cents
        };
        assert!(close(cost_on("2026-06-30"), 14.0 * 15.0 + 6.0 * 5.0));
        assert!(close(cost_on("2026-07-01"), 20.0 * 5.0));
    }

    /// A flat plan with an export tier and a credit, optionally dynamic.
    fn plan_with(dynamic: &str) -> Tariff {
        tariff(&format!(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 30.0
            default_export_cents = 5.0
            import = [{{ from = "00:00", to = "01:00", cents = 10.0 }}]
            zero_import_credit = {{ from = "23:00", to = "23:59", max_kwh = 1.0, credit_cents = 50.0 }}
            {dynamic}
            "#
        ))
    }

    /// Two 5-minute intervals from midnight on 1 Sep 2026 (Adelaide, UTC+9:30).
    fn two_intervals() -> (i64, Vec<Interval>) {
        let midnight = ADELAIDE
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let iv = |i: i64| Interval {
            start_ts: midnight + i * 300,
            end_ts: midnight + (i + 1) * 300,
            import_kwh: 2.0,
            export_kwh: 1.0,
            load_kwh: 3.0,
        };
        (midnight, vec![iv(0), iv(1)])
    }

    fn span(start_ts: i64, cents: f64) -> PriceSpan {
        PriceSpan {
            start_ts,
            end_ts: start_ts + 300,
            cents,
        }
    }

    const DYNAMIC: &str = "[tariff.dynamic]\nsource = \"amber\"\nfee_cents_per_day = 20.0\n";

    #[test]
    fn without_prices_the_static_path_is_unchanged() {
        let t = plan_with("");
        let (_, ivs) = two_intervals();
        let plain = day_cost_with_window_import(&t, ADELAIDE, &ivs, None);
        let priced = day_cost_priced(&t, ADELAIDE, &ivs, None, None);
        assert_eq!(priced.cost, plain);
        assert_eq!(priced.fallback_minutes, 0);
        // Golden numbers: 2 x 2 kWh at the 10c window, 2 x 1 kWh at 5c.
        assert_eq!(plain.import_cents, 40.0);
        assert_eq!(plain.export_cents, 10.0);
        assert_eq!(plain.supply_cents, 100.0);
    }

    #[test]
    fn a_dynamic_day_is_priced_from_the_rows() {
        let t = plan_with(DYNAMIC);
        let (m, ivs) = two_intervals();
        let prices = DayPrices::new(
            vec![span(m, 40.0), span(m + 300, 20.0)],
            vec![span(m, -8.0), span(m + 300, -2.0)],
        );
        let r = day_cost_priced(&t, ADELAIDE, &ivs, Some(&prices), None);
        // Import: 2 kWh at 40c and at 20c. Feed-in: 1 kWh at +8c and +2c.
        assert_eq!(r.cost.import_cents, 120.0);
        assert_eq!(r.cost.export_cents, 10.0);
        // The retailer fee is added to the supply charge.
        assert_eq!(r.cost.supply_cents, 120.0);
        assert_eq!(r.fallback_minutes, 0);
        // The baseline (no solar or battery) uses the same live prices.
        assert_eq!(r.cost.baseline_cents, 120.0 + 3.0 * 40.0 + 3.0 * 20.0);
    }

    #[test]
    fn missing_rows_fall_back_to_the_windows_and_are_counted() {
        let t = plan_with(DYNAMIC);
        let (m, ivs) = two_intervals();
        // Only the first interval has prices; the second uses the plan's
        // 10c import window and 5c default export.
        let prices = DayPrices::new(vec![span(m, 40.0)], vec![span(m, -8.0)]);
        let r = day_cost_priced(&t, ADELAIDE, &ivs, Some(&prices), None);
        assert_eq!(r.cost.import_cents, 2.0 * 40.0 + 2.0 * 10.0);
        assert_eq!(r.cost.export_cents, 8.0 + 5.0);
        assert_eq!(r.fallback_minutes, 5);
        // No rows at all: everything falls back.
        let none = day_cost_priced(&t, ADELAIDE, &ivs, Some(&DayPrices::default()), None);
        assert_eq!(none.fallback_minutes, 10);
        assert_eq!(none.cost.import_cents, 40.0);
    }

    #[test]
    fn the_sign_setting_decides_what_a_feed_in_price_means() {
        let (m, ivs) = two_intervals();
        let prices = DayPrices::new(
            vec![span(m, 10.0), span(m + 300, 10.0)],
            vec![span(m, -8.0), span(m + 300, 3.0)],
        );
        let flipped = plan_with(DYNAMIC);
        let r = day_cost_priced(&flipped, ADELAIDE, &ivs, Some(&prices), None);
        // -8 earns 8 and +3 costs 3.
        assert_eq!(r.cost.export_cents, 8.0 - 3.0);

        let as_is = plan_with(&format!("{DYNAMIC}feed_in_paid_when_negative = false\n"));
        let r = day_cost_priced(&as_is, ADELAIDE, &ivs, Some(&prices), None);
        assert_eq!(r.cost.export_cents, -8.0 + 3.0);
    }

    #[test]
    fn the_export_tier_still_applies_on_a_dynamic_plan() {
        let t = plan_with(&format!(
            "{DYNAMIC}\n[tariff.export_tier]\nfirst_kwh_per_day = 1.0\ncents = 12.0\n"
        ));
        let (m, ivs) = two_intervals();
        let prices = DayPrices::new(
            vec![span(m, 10.0), span(m + 300, 10.0)],
            vec![span(m, -8.0), span(m + 300, -2.0)],
        );
        let r = day_cost_priced(&t, ADELAIDE, &ivs, Some(&prices), None);
        // The first exported kWh earns the tier's 12c; the second the live 2c.
        assert_eq!(r.cost.export_cents, 12.0 + 2.0);
    }

    #[test]
    fn a_dynamic_plan_still_earns_the_credit() {
        let t = plan_with(DYNAMIC);
        let late = ADELAIDE
            .with_ymd_and_hms(2026, 9, 1, 23, 40, 0)
            .unwrap()
            .timestamp();
        let iv = Interval {
            start_ts: late,
            end_ts: late + 1140,
            import_kwh: 0.0,
            export_kwh: 0.0,
            load_kwh: 0.1,
        };
        let r = day_cost_priced(&t, ADELAIDE, &[iv], Some(&DayPrices::default()), None);
        assert_eq!(r.cost.zero_import_credit_cents, 50.0);
    }

    #[test]
    fn live_prices_band_by_the_spike_setting() {
        assert_eq!(live_import_band(-1.0, 100.0), ImportBand::Free);
        assert_eq!(live_import_band(0.0, 100.0), ImportBand::Free);
        assert_eq!(live_import_band(100.0, 100.0), ImportBand::Shoulder);
        assert_eq!(live_import_band(100.1, 100.0), ImportBand::Peak);
        let t = plan_with(DYNAMIC);
        let (m, ivs) = two_intervals();
        let prices = DayPrices::new(
            vec![span(m, 0.0), span(m + 300, 150.0)],
            vec![span(m, -8.0), span(m + 300, 3.0)],
        );
        let split = grid_split_priced(&t, ADELAIDE, &ivs, Some(&prices), 100.0);
        assert_eq!(split.import_free_kwh, 2.0);
        assert_eq!(split.import_peak_kwh, 2.0);
        assert_eq!(split.export_peak_kwh, 1.0);
        assert_eq!(split.export_other_kwh, 1.0);
        // Without prices it is the plain split.
        assert_eq!(
            grid_split_priced(&t, ADELAIDE, &ivs, None, 100.0),
            grid_split(&t, ADELAIDE, &ivs)
        );
    }

    #[test]
    fn a_static_plan_keeps_its_stored_json_and_key() {
        let t = plan_with("");
        // Written out in full: this is the key every static plan had before
        // dynamic prices existed, so stored costs stay valid.
        assert_eq!(
            t.key(),
            r#"{"supply_cents_per_day":100.0,"default_import_cents":30.0,"import":[{"from":"00:00","to":"01:00","cents":10.0}],"default_export_cents":5.0,"export":[],"zero_import_credit":{"from":"23:00","to":"23:59","max_kwh":1.0,"credit_cents":50.0}}"#
        );
        assert!(!t.key().contains("dynamic"));
        // A dynamic plan has its own key, so its costs are kept apart.
        assert!(
            plan_with(DYNAMIC)
                .key()
                .contains(r#""dynamic":{"source":"amber""#)
        );
    }
}
