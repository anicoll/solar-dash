//! Electricity cost calculation. Pure functions only: no database, no network,
//! so every rule here can be tested with plain numbers.

// Nothing calls this module yet: the API that uses it comes in a later commit.
#![allow(dead_code)]

use anyhow::{Context, Result};
use chrono::{DateTime, Timelike};
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
}

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
        })
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

/// Local minutes since midnight for a unix time. We price an interval at its
/// midpoint, so an interval straddling a price change is not split.
fn local_minute_of_day(tz: Tz, ts: i64) -> u32 {
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
    // Import inside the credit window, and whether we saw any interval there.
    // Without data we cannot claim the grid was unused.
    let mut window_import_kwh = 0.0;
    let mut window_has_data = false;
    let mut baseline_import_cents = 0.0;

    for iv in intervals {
        // Sum then halve, so we cannot overflow on huge timestamps.
        let mid = iv.start_ts + (iv.end_ts - iv.start_ts) / 2;
        let minute = local_minute_of_day(tz, mid);
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
            window_has_data = true;
        }
    }

    if let Some(credit) = &tariff.zero_import_credit
        && window_has_data
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
                iv((22, 0), (23, 0), 5.0, 0.0, 5.0),  // outside the window
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
            ],
        );
        assert_eq!(day.zero_import_credit_cents, 0.0);
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
}
