//! Where the home's power came from over a day: solar first, then the
//! battery, then the grid for whatever is left.
//!
//! `1 - import / load` treats every kWh bought as if it fed the house before
//! solar did. On a day with free import, most of what is bought goes into the
//! battery and the cars, so that figure looks far lower than the share of the
//! house that really ran on its own power. Here each moment's load is covered
//! by solar made at that moment first, then battery discharge, then the grid,
//! the same order the Now view and the car charging split use.

use chrono_tz::Tz;
use serde::Serialize;

use super::api::{MAX_HOLD_SECS, MAX_SPREAD_SECS, derive_power};
use crate::cost::{ImportBand, Tariff, local_minute_of_day};
use crate::modbus::Sample;

/// W·s in one kWh.
const WS_PER_KWH: f64 = 3.6e6;

/// Power held steady from `start` for `secs` seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Step {
    pub start: i64,
    pub secs: i64,
    pub pv_w: f64,
    pub load_w: f64,
    /// Battery discharge, as a positive number (0 while charging).
    pub discharge_w: f64,
}

/// The home load of a day by where it came from, in kWh. The first three add
/// up to the load.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize)]
pub(super) struct HomeMix {
    #[serde(rename = "home_solar_kwh")]
    pub solar_kwh: Option<f64>,
    #[serde(rename = "home_battery_kwh")]
    pub battery_kwh: Option<f64>,
    #[serde(rename = "home_grid_kwh")]
    pub grid_kwh: Option<f64>,
    /// The part of `grid_kwh` bought in a free import band. `None` without a tariff.
    #[serde(rename = "home_grid_free_kwh")]
    pub grid_free_kwh: Option<f64>,
}

/// Splits the load of `steps` by source: solar first, then battery, then grid.
/// The grid part bought while `tariff` makes import free (judged at each
/// step's midpoint, as the bill does) is also counted in `grid_free_kwh`.
/// No steps means no figures at all.
pub(super) fn home_mix(steps: &[Step], tariff: Option<&Tariff>, tz: Tz) -> HomeMix {
    if steps.is_empty() {
        return HomeMix::default();
    }
    let (mut solar, mut battery, mut grid, mut free) = (0.0, 0.0, 0.0, 0.0);
    for step in steps {
        let load = step.load_w.max(0.0);
        let from_solar = step.pv_w.max(0.0).min(load);
        let from_battery = step.discharge_w.max(0.0).min(load - from_solar);
        let from_grid = load - from_solar - from_battery;
        let secs = step.secs as f64;
        solar += from_solar * secs;
        battery += from_battery * secs;
        grid += from_grid * secs;
        let mid = step.start + step.secs / 2;
        if tariff
            .is_some_and(|t| t.import_band_at(local_minute_of_day(tz, mid)) == ImportBand::Free)
        {
            free += from_grid * secs;
        }
    }
    HomeMix {
        solar_kwh: Some(solar / WS_PER_KWH),
        battery_kwh: Some(battery / WS_PER_KWH),
        grid_kwh: Some(grid / WS_PER_KWH),
        grid_free_kwh: tariff.map(|_| free / WS_PER_KWH),
    }
}

/// Steps from live samples (oldest first). Each sample is held until the next
/// one, for at most [`MAX_HOLD_SECS`], as the day totals do. A sample with no
/// load reading is skipped; a missing solar or battery reading counts as 0.
pub(super) fn live_steps(samples: &[Sample]) -> Vec<Step> {
    samples
        .windows(2)
        .filter_map(|pair| {
            let s = &pair[0];
            Some(Step {
                start: s.ts,
                secs: (pair[1].ts - s.ts).min(MAX_HOLD_SECS),
                pv_w: s.pv_w.unwrap_or(0.0),
                load_w: s.load_w?,
                discharge_w: (-s.battery_w.unwrap_or(0.0)).max(0.0),
            })
        })
        .collect()
}

/// Steps from the cloud's daily counters (`(ts, Wh)`, oldest first, already
/// moved back to when they were read): solar yield, load and battery
/// discharge. Each load reading gives one step from the reading before it, at
/// most [`MAX_SPREAD_SECS`] long. Solar and discharge are taken from their own
/// counters over the span that covers the step's end (0 when there is none).
pub(super) fn cloud_steps(
    pv: &[(i64, f64)],
    load: &[(i64, f64)],
    discharge: &[(i64, f64)],
) -> Vec<Step> {
    let pv = derive_power(pv);
    let discharge = derive_power(discharge);
    // The power of a derived series at `ts`: its first row at or after `ts`
    // (a row's value is the average over the time up to it).
    let at = |rows: &[(i64, f64)], ts: i64| {
        let i = rows.partition_point(|row| row.0 < ts);
        rows.get(i).map_or(0.0, |row| row.1)
    };
    // `derive_power` gives a row for each reading after the first.
    derive_power(load)
        .iter()
        .zip(load)
        .map(|(&(ts, load_w), &(prev_ts, _))| {
            let secs = (ts - prev_ts).min(MAX_SPREAD_SECS);
            Step {
                start: ts - secs,
                secs,
                pv_w: at(&pv, ts),
                load_w,
                discharge_w: at(&discharge, ts),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    fn step(start: i64, pv_w: f64, load_w: f64, discharge_w: f64) -> Step {
        Step {
            start,
            secs: 3600,
            pv_w,
            load_w,
            discharge_w,
        }
    }

    #[test]
    fn solar_covers_the_home_before_the_grid() {
        // 5 kW of solar and an 11 kW load (car charging) while 10 kW is
        // bought to fill the battery as well: the house still ran 5 kWh on
        // solar, and only the other 6 kWh came from the grid.
        let mix = home_mix(&[step(0, 5000.0, 11_000.0, 0.0)], None, ADELAIDE);
        assert_eq!(mix.solar_kwh, Some(5.0));
        assert_eq!(mix.battery_kwh, Some(0.0));
        assert_eq!(mix.grid_kwh, Some(6.0));
        assert_eq!(mix.grid_free_kwh, None);
    }

    #[test]
    fn battery_comes_after_solar_and_spare_solar_is_not_counted() {
        let mix = home_mix(
            &[
                // More solar than load: only the load counts.
                step(0, 4000.0, 1000.0, 0.0),
                // Evening: 0.5 kW solar, battery covers the rest of 2 kW.
                step(3600, 500.0, 2000.0, 3000.0),
                // Battery too small: the grid tops up.
                step(7200, 0.0, 2000.0, 1500.0),
            ],
            None,
            ADELAIDE,
        );
        assert_eq!(mix.solar_kwh, Some(1.5));
        assert_eq!(mix.battery_kwh, Some(3.0));
        assert_eq!(mix.grid_kwh, Some(0.5));
    }

    #[test]
    fn grid_in_the_free_window_is_counted_as_free() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: crate::config::Tariff,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 40.0
            import = [{ from = "11:00", to = "14:00", cents = 0.0 }]
            default_export_cents = 0.0
            "#,
        )
        .unwrap();
        let tariff = Tariff::from_config(&w.tariff).unwrap();
        // 2026-10-20 12:00 and 15:00 in Adelaide (UTC+10:30).
        let noon = 1_760_923_800;
        let mix = home_mix(
            &[
                step(noon, 1000.0, 3000.0, 0.0),
                step(noon + 3 * 3600, 0.0, 1000.0, 0.0),
            ],
            Some(&tariff),
            ADELAIDE,
        );
        assert_eq!(mix.grid_kwh, Some(3.0));
        assert_eq!(mix.grid_free_kwh, Some(2.0));
    }

    #[test]
    fn no_steps_means_no_figures() {
        assert_eq!(home_mix(&[], None, ADELAIDE), HomeMix::default());
    }

    #[test]
    fn live_steps_hold_each_sample_and_skip_missing_load() {
        let s = |ts, load_w, battery_w| Sample {
            ts,
            pv_w: Some(100.0),
            load_w,
            grid_w: None,
            battery_w,
            soc_pct: None,
        };
        let steps = live_steps(&[
            s(0, Some(500.0), Some(-300.0)),
            s(15, None, Some(200.0)),
            s(1000, Some(1.0), None),
        ]);
        assert_eq!(
            steps,
            vec![Step {
                start: 0,
                secs: 15,
                pv_w: 100.0,
                load_w: 500.0,
                discharge_w: 300.0,
            }]
        );
    }

    #[test]
    fn cloud_steps_line_up_the_counters() {
        // 5-minute readings: 100 Wh of load in each, 50 Wh of solar by the
        // second (spread over the 10 minutes since that counter last moved,
        // so 300 W in both steps), no discharge.
        let load = [(0, 0.0), (300, 100.0), (600, 200.0)];
        let pv = [(0, 0.0), (300, 0.0), (600, 50.0)];
        let steps = cloud_steps(&pv, &load, &[]);
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].start, steps[0].secs), (0, 300));
        assert_eq!(steps[0].load_w, 1200.0);
        assert_eq!((steps[0].pv_w, steps[1].pv_w), (300.0, 300.0));
        assert_eq!(steps[1].discharge_w, 0.0);
    }
}
