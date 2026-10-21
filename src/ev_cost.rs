//! Where a charging session's energy came from (solar, the home battery or
//! the grid) and what it cost. Pure functions only: no database, no network,
//! so the rules can be tested with plain numbers.
//!
//! The car's own power is not stored. It sits behind the house meter, so it
//! is already part of the house load in the live samples. We cannot see which
//! watts went to the car, so we give it the same mix as the whole house load.

use chrono::{DateTime, NaiveDate, Timelike};
use chrono_tz::Tz;

use crate::cost::TariffSchedule;
use crate::db::ChargeSession;
use crate::modbus::Sample;

/// A longer gap between two live samples is unknown time: it adds no energy
/// and does not count as covered. Samples come every ~15 s, so a longer gap
/// means the dongle dropped out.
const MAX_GAP_SECS: i64 = 120;
/// W·s in one kWh.
const WS_PER_KWH: f64 = 3.6e6;

/// The energy of one session by source, and its price. Only the time the live
/// samples cover is counted (see `coverage`); the rest is not guessed.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionCost {
    pub solar_kwh: f64,
    pub battery_kwh: f64,
    pub grid_kwh: f64,
    /// What the grid part cost at the import price, in cents.
    pub cost_cents: f64,
    /// What the solar part would have earned if it had been exported
    /// instead, in cents. Kept apart from `cost_cents`: it is money not
    /// earned, not money spent.
    pub solar_value_cents: f64,
    /// The share of the session (0 to 1) that live samples cover.
    pub coverage: f64,
}

/// Local date and minutes since midnight for a unix time, or `None` for an
/// absurd time far outside real data.
fn local_time(tz: Tz, ts: i64) -> Option<(NaiveDate, u32)> {
    let local = DateTime::from_timestamp(ts, 0)?.with_timezone(&tz);
    Some((local.date_naive(), local.hour() * 60 + local.minute()))
}

/// Works out one session from the live `samples` (oldest first, any range
/// that includes the session).
///
/// The car's average power is its energy over the session's length: the
/// energy used from the wall, or the energy added to the battery if that is
/// missing. For each sample, held until the next one:
///
/// 1. The car draws that average, but never more than the house load.
/// 2. The house load is split like the Now view does: solar first, then
///    battery discharge, then the grid. Whatever those two do not cover is
///    counted as grid.
/// 3. The car gets the same share of each source as the whole load does.
///
/// The grid part is priced at the import price of the sample's local time,
/// on the plan in effect that day. A sample with no load reading, and a gap
/// longer than [`MAX_GAP_SECS`], are skipped.
///
/// Returns `None` if the session has no energy or no length to work from.
pub fn session_cost(
    session: &ChargeSession,
    samples: &[Sample],
    schedule: &TariffSchedule,
    tz: Tz,
    now: i64,
) -> Option<SessionCost> {
    let start = session.started_at;
    let end = session.ended_at.unwrap_or(now);
    let energy_kwh = session
        .energy_used_kwh
        .or(session.energy_added_kwh)
        .filter(|kwh| *kwh > 0.0)?;
    if end <= start {
        return None;
    }
    let car_avg_w = energy_kwh * WS_PER_KWH / (end - start) as f64;

    let mut cost = SessionCost {
        solar_kwh: 0.0,
        battery_kwh: 0.0,
        grid_kwh: 0.0,
        cost_cents: 0.0,
        solar_value_cents: 0.0,
        coverage: 0.0,
    };
    let mut covered_secs = 0;
    for pair in samples.windows(2) {
        // Only the part of the interval inside the session counts.
        let (from, to) = (pair[0].ts.max(start), pair[1].ts.min(end));
        if pair[1].ts - pair[0].ts > MAX_GAP_SECS || to <= from {
            continue;
        }
        let Some(load_w) = pair[0].load_w.map(|w| w.max(0.0)) else {
            continue;
        };
        // Price at the middle of the interval, like `cost::day_cost` does.
        let Some((date, minute)) = local_time(tz, from + (to - from) / 2) else {
            continue;
        };
        covered_secs += to - from;

        let pv_w = pair[0].pv_w.unwrap_or(0.0).max(0.0);
        let discharge_w = (-pair[0].battery_w.unwrap_or(0.0)).max(0.0);
        let solar_w = pv_w.min(load_w);
        let battery_w = discharge_w.min(load_w - solar_w);
        let grid_w = load_w - solar_w - battery_w;

        // The car's share of the load, and so of each source.
        let share = if load_w > 0.0 {
            car_avg_w.min(load_w) / load_w
        } else {
            0.0
        };
        let to_kwh = |watts: f64| watts * share * (to - from) as f64 / WS_PER_KWH;
        let tariff = schedule.for_date(date);
        cost.solar_kwh += to_kwh(solar_w);
        cost.battery_kwh += to_kwh(battery_w);
        cost.grid_kwh += to_kwh(grid_w);
        cost.cost_cents += to_kwh(grid_w) * tariff.import_cents_at(minute);
        cost.solar_value_cents += to_kwh(solar_w) * tariff.export_cents_at(minute);
    }
    cost.coverage = covered_secs as f64 / (end - start) as f64;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use chrono::TimeZone;

    const ADELAIDE: Tz = chrono_tz::Australia::Adelaide;

    /// The ZeroHero-like plan: free from 11:00 to 14:00, 58.3 c from 15:00 to
    /// 23:00, 44 c otherwise, and 5 c for exporting.
    fn schedule() -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 44.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "15:00", to = "23:00", cents = 58.3 },
            ]
            default_export_cents = 5.0
            "#,
        )
        .unwrap();
        TariffSchedule::from_config(&w.tariff, &[]).unwrap()
    }

    /// Unix time of a local Adelaide clock time on 2026-09-15 (no DST change
    /// that day, so +09:30).
    fn at(hour: u32, minute: u32) -> i64 {
        ADELAIDE
            .with_ymd_and_hms(2026, 9, 15, hour, minute, 0)
            .unwrap()
            .timestamp()
    }

    /// A session from `start` for `hours`, with 7.4 kWh taken from the wall.
    fn session(start: i64, hours: i64) -> ChargeSession {
        ChargeSession {
            id: 1,
            started_at: start,
            ended_at: Some(start + hours * 3600),
            energy_added_kwh: Some(6.8),
            energy_used_kwh: Some(7.4),
            start_soc_pct: None,
            end_soc_pct: None,
            max_power_kw: None,
            geofence: Some("Home".to_string()),
        }
    }

    /// A sample with the car's 7.4 kW inside the load.
    fn sample(ts: i64, pv_w: f64, load_w: f64, grid_w: f64, battery_w: f64) -> Sample {
        Sample {
            ts,
            pv_w: Some(pv_w),
            load_w: Some(load_w),
            grid_w: Some(grid_w),
            battery_w: Some(battery_w),
            soc_pct: None,
        }
    }

    /// Samples every 15 s from `from` to `to`, all the same.
    fn steady(from: i64, to: i64, s: Sample) -> Vec<Sample> {
        (from..=to)
            .step_by(15)
            .map(|ts| Sample { ts, ..s })
            .collect()
    }

    fn close(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-6, "got {got}, wanted {want}");
    }

    #[test]
    fn all_solar_session_costs_nothing_and_has_a_feed_in_value() {
        let (from, to) = (at(10, 0), at(11, 0));
        // Plenty of sun for the house and the car.
        let samples = steady(from, to, sample(0, 9000.0, 8000.0, -1000.0, 0.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.solar_kwh, 7.4);
        close(got.battery_kwh, 0.0);
        close(got.grid_kwh, 0.0);
        close(got.cost_cents, 0.0);
        // 7.4 kWh at the 5 c export price.
        close(got.solar_value_cents, 37.0);
        close(got.coverage, 1.0);
    }

    #[test]
    fn all_grid_session_at_peak_price() {
        let (from, to) = (at(18, 0), at(19, 0));
        let samples = steady(from, to, sample(0, 0.0, 8000.0, 8000.0, 0.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.grid_kwh, 7.4);
        close(got.solar_kwh, 0.0);
        close(got.cost_cents, 7.4 * 58.3);
        close(got.solar_value_cents, 0.0);
    }

    #[test]
    fn session_in_the_free_window_costs_nothing() {
        let (from, to) = (at(12, 0), at(13, 0));
        let samples = steady(from, to, sample(0, 0.0, 8000.0, 8000.0, 0.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.grid_kwh, 7.4);
        close(got.cost_cents, 0.0);
    }

    #[test]
    fn the_car_gets_the_same_mix_as_the_whole_load() {
        let (from, to) = (at(18, 0), at(19, 0));
        // Load 8 kW: 2 kW solar, 2 kW from the battery, 4 kW from the grid.
        let samples = steady(from, to, sample(0, 2000.0, 8000.0, 4000.0, -2000.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.solar_kwh, 7.4 * 0.25);
        close(got.battery_kwh, 7.4 * 0.25);
        close(got.grid_kwh, 7.4 * 0.5);
        close(got.cost_cents, 7.4 * 0.5 * 58.3);
    }

    #[test]
    fn the_car_never_draws_more_than_the_load() {
        let (from, to) = (at(18, 0), at(19, 0));
        // The session averaged 7.4 kW, but the house only used 2 kW.
        let samples = steady(from, to, sample(0, 0.0, 2000.0, 2000.0, 0.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.grid_kwh, 2.0);
    }

    #[test]
    fn a_gap_in_the_samples_is_unknown_and_lowers_coverage() {
        let start = at(18, 0);
        let mut samples = steady(start, start + 1800, sample(0, 0.0, 8000.0, 8000.0, 0.0));
        // Then 10 minutes with no samples at all.
        samples.extend(steady(
            start + 2400,
            start + 3600,
            sample(0, 0.0, 8000.0, 8000.0, 0.0),
        ));
        let got = session_cost(
            &session(start, 1),
            &samples,
            &schedule(),
            ADELAIDE,
            start + 3600,
        )
        .unwrap();
        close(got.coverage, 3000.0 / 3600.0);
        // Only the 3000 s with samples is counted, not guessed.
        close(got.grid_kwh, 7.4 * 3000.0 / 3600.0);
    }

    #[test]
    fn no_samples_or_no_energy_gives_no_cost() {
        let start = at(18, 0);
        let none = session_cost(&session(start, 1), &[], &schedule(), ADELAIDE, start).unwrap();
        close(none.coverage, 0.0);
        close(none.grid_kwh, 0.0);

        let mut empty = session(start, 1);
        empty.energy_used_kwh = None;
        empty.energy_added_kwh = None;
        assert_eq!(
            session_cost(&empty, &[], &schedule(), ADELAIDE, start),
            None
        );
        let instant = session(start, 0);
        assert_eq!(
            session_cost(&instant, &[], &schedule(), ADELAIDE, start),
            None
        );
    }

    #[test]
    fn energy_added_is_used_when_the_energy_used_is_missing() {
        let (from, to) = (at(18, 0), at(19, 0));
        let samples = steady(from, to, sample(0, 0.0, 8000.0, 8000.0, 0.0));
        let mut old = session(from, 1);
        old.energy_used_kwh = None;
        let got = session_cost(&old, &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.grid_kwh, 6.8);
    }

    #[test]
    fn a_running_session_lasts_until_now() {
        let start = at(12, 0);
        let samples = steady(start, start + 7200, sample(0, 0.0, 8000.0, 8000.0, 0.0));
        let mut running = session(start, 1);
        running.ended_at = None;
        // 30 minutes in, with 3.7 kWh drawn so far.
        running.energy_used_kwh = Some(3.7);
        let got = session_cost(&running, &samples, &schedule(), ADELAIDE, start + 1800).unwrap();
        close(got.coverage, 1.0);
        close(got.grid_kwh, 3.7);
    }
}
