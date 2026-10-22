//! Where a charging session's energy came from (solar, the home battery or
//! the grid) and what it cost. Pure functions only: no database, no network,
//! so the rules can be tested with plain numbers. It also adds sessions and
//! km up into the car-versus-petrol summary.
//!
//! The car's own power is not stored. It sits behind the house meter, so it
//! is already part of the house load in the live samples. We cannot see which
//! watts went to the car, so we give it the same mix as the whole house load,
//! with solar charging the home battery before it reaches the load.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Timelike};
use chrono_tz::Tz;
use serde::Serialize;

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

/// The energy a session took, kWh: from the wall if we know it, otherwise
/// what reached the battery. `None` if there is none.
pub fn session_energy_kwh(session: &ChargeSession) -> Option<f64> {
    session
        .energy_used_kwh
        .or(session.energy_added_kwh)
        .filter(|kwh| *kwh > 0.0)
}

/// Whether a session was at home: the geofence "Home", or none at all.
pub fn is_home(session: &ChargeSession) -> bool {
    matches!(session.geofence.as_deref(), None | Some("Home"))
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
    let energy_kwh = session_energy_kwh(session)?;
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
        let battery_now_w = pair[0].battery_w.unwrap_or(0.0);
        let discharge_w = (-battery_now_w).max(0.0);
        // Solar charges the battery first (it sits on the inverter's DC
        // side, as the Sungrow app shows it); only the rest reaches the load.
        let solar_w = (pv_w - battery_now_w.max(0.0)).max(0.0).min(load_w);
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

/// What the comparison with petrol is based on; see `[teslamate]` in
/// config.example.toml.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvPrices {
    /// `None` turns the comparison off.
    pub petrol_cents_per_litre: Option<f64>,
    pub petrol_litres_per_100km: f64,
    /// `None` leaves sessions away from home uncosted.
    pub away_cents_per_kwh: Option<f64>,
}

impl Default for EvPrices {
    fn default() -> Self {
        Self {
            petrol_cents_per_litre: None,
            petrol_litres_per_100km: 7.5,
            away_cents_per_kwh: None,
        }
    }
}

/// One charging session, ready to be added up.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEnergy {
    /// The local month it started in, "YYYY-MM".
    pub month: String,
    pub at_home: bool,
    pub kwh: f64,
    /// What it cost, in cents, or `None` if it cannot be costed.
    pub cost_cents: Option<f64>,
}

/// The km, charging and petrol numbers of one month or of the whole range.
/// Money is in dollars. A money field is `None` when it cannot be worked out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Period {
    pub km: f64,
    pub home_kwh: f64,
    pub home_cost: Option<f64>,
    pub away_kwh: f64,
    /// `None` when there were away sessions and no `away_cents_per_kwh`.
    pub away_cost: Option<f64>,
    /// Home plus away. Away sessions that are not costed count as nothing,
    /// so look at `away_cost` before trusting a big saving.
    pub charging_cost: Option<f64>,
    pub petrol_litres: Option<f64>,
    pub petrol_cost: Option<f64>,
    /// Petrol cost minus charging cost.
    pub saved: Option<f64>,
    /// `None` when the car did not drive.
    pub ev_cents_per_km: Option<f64>,
    pub petrol_cents_per_km: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthSummary {
    /// "YYYY-MM"
    pub month: String,
    #[serde(flatten)]
    pub period: Period,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// False when there is no petrol price: the money fields are then null.
    pub configured: bool,
    pub months: Vec<MonthSummary>,
    pub total: Period,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The cost of some sessions in cents, or `None` if any of them has none.
fn total_cost_cents(sessions: &[&SessionEnergy]) -> Option<f64> {
    sessions.iter().map(|s| s.cost_cents).sum()
}

/// Adds up one period's `km` and `sessions`.
fn period(prices: &EvPrices, km: f64, sessions: &[&SessionEnergy]) -> Period {
    let (home, away): (Vec<_>, Vec<_>) = sessions.iter().copied().partition(|s| s.at_home);
    let kwh = |sessions: &[&SessionEnergy]| sessions.iter().map(|s| s.kwh).sum::<f64>();
    let mut period = Period {
        km: round2(km),
        home_kwh: round2(kwh(&home)),
        home_cost: None,
        away_kwh: round2(kwh(&away)),
        away_cost: None,
        charging_cost: None,
        petrol_litres: None,
        petrol_cost: None,
        saved: None,
        ev_cents_per_km: None,
        petrol_cents_per_km: None,
    };
    let Some(petrol_cents) = prices.petrol_cents_per_litre else {
        return period;
    };

    let home_cost = total_cost_cents(&home);
    let away_cost = total_cost_cents(&away);
    let charging_cost = home_cost.map(|home| home + away_cost.unwrap_or(0.0));
    let litres = km * prices.petrol_litres_per_100km / 100.0;
    let petrol_cost = litres * petrol_cents;
    period.home_cost = home_cost.map(|c| round2(c / 100.0));
    period.away_cost = away_cost.map(|c| round2(c / 100.0));
    period.charging_cost = charging_cost.map(|c| round2(c / 100.0));
    period.petrol_litres = Some(round2(litres));
    period.petrol_cost = Some(round2(petrol_cost / 100.0));
    period.saved = charging_cost.map(|c| round2((petrol_cost - c) / 100.0));
    if km > 0.0 {
        period.ev_cents_per_km = charging_cost.map(|c| round2(c / km));
        period.petrol_cents_per_km = Some(round2(petrol_cost / km));
    }
    period
}

/// Adds the sessions and km up by month, and for the whole range.
/// `km_by_month` is from [`monthly_km`]. A month with km or sessions gets a
/// row, oldest first. Without a petrol price the money fields are all `None`.
pub fn summarise(
    prices: &EvPrices,
    km_by_month: &BTreeMap<String, f64>,
    sessions: &[SessionEnergy],
) -> Summary {
    let mut by_month: BTreeMap<&str, Vec<&SessionEnergy>> = BTreeMap::new();
    for month in km_by_month.keys() {
        by_month.entry(month).or_default();
    }
    for session in sessions {
        by_month.entry(&session.month).or_default().push(session);
    }
    let months = by_month
        .into_iter()
        .map(|(month, sessions)| MonthSummary {
            month: month.to_string(),
            period: period(
                prices,
                km_by_month.get(month).copied().unwrap_or(0.0),
                &sessions,
            ),
        })
        .collect();
    let all: Vec<&SessionEnergy> = sessions.iter().collect();
    let total_km = km_by_month.values().sum();
    Summary {
        configured: prices.petrol_cents_per_litre.is_some(),
        months,
        total: period(prices, total_km, &all),
    }
}

/// The km driven in each month, from the odometer readings (local date
/// "YYYY-MM-DD" and km, oldest first). A day's km is its reading minus the
/// reading before it, so a day with no reading passes its km on to the next
/// day with one. Only days from `from` on are counted: the reading before
/// that is just the starting point. The very first reading has nothing to
/// compare with, and a reading that goes down (a replaced odometer) is skipped.
pub fn monthly_km(readings: &[(String, f64)], from: &str) -> BTreeMap<String, f64> {
    let mut months = BTreeMap::new();
    for pair in readings.windows(2) {
        let (date, km) = (&pair[1].0, pair[1].1 - pair[0].1);
        if date.as_str() >= from && km >= 0.0 {
            *months.entry(date[..7].to_string()).or_insert(0.0) += km;
        }
    }
    months
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
    fn solar_charges_the_battery_before_it_reaches_the_car() {
        let (from, to) = (at(18, 0), at(19, 0));
        // 6 kW of sun, 3 kW into the battery, load 8 kW: 3 kW solar, 5 kW grid.
        let samples = steady(from, to, sample(0, 6000.0, 8000.0, 5000.0, 3000.0));
        let got = session_cost(&session(from, 1), &samples, &schedule(), ADELAIDE, to).unwrap();
        close(got.solar_kwh, 7.4 * 3.0 / 8.0);
        close(got.battery_kwh, 0.0);
        close(got.grid_kwh, 7.4 * 5.0 / 8.0);
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

    fn prices() -> EvPrices {
        EvPrices {
            petrol_cents_per_litre: Some(200.0),
            petrol_litres_per_100km: 8.0,
            away_cents_per_kwh: Some(60.0),
        }
    }

    fn charge(month: &str, at_home: bool, kwh: f64, cost_cents: Option<f64>) -> SessionEnergy {
        SessionEnergy {
            month: month.to_string(),
            at_home,
            kwh,
            cost_cents,
        }
    }

    fn readings(rows: &[(&str, f64)]) -> Vec<(String, f64)> {
        rows.iter().map(|(d, km)| (d.to_string(), *km)).collect()
    }

    #[test]
    fn km_is_the_difference_from_the_previous_reading() {
        let rows = readings(&[
            ("2026-08-30", 1000.0),
            ("2026-09-01", 1030.0),
            // Nothing on the 2nd to the 4th: the 5th gets all of it.
            ("2026-09-05", 1100.0),
            ("2026-10-01", 1150.0),
        ]);
        let km = monthly_km(&rows, "2026-09-01");
        assert_eq!(km["2026-09"], 100.0);
        assert_eq!(km["2026-10"], 50.0);
        // The 30th of August is before the range: only a starting point.
        assert_eq!(km.len(), 2);
        // Without an earlier reading the first day has no km.
        let km = monthly_km(&rows, "2026-08-01");
        assert_eq!(km.get("2026-08"), None);
        assert_eq!(km["2026-09"], 100.0);
    }

    #[test]
    fn km_skips_a_reading_that_goes_down() {
        let rows = readings(&[
            ("2026-09-01", 5000.0),
            ("2026-09-02", 12.0),
            ("2026-09-03", 20.0),
        ]);
        assert_eq!(monthly_km(&rows, "2026-09-01")["2026-09"], 8.0);
    }

    #[test]
    fn summary_compares_charging_with_petrol() {
        let km = BTreeMap::from([
            ("2026-09".to_string(), 400.0),
            ("2026-10".to_string(), 100.0),
        ]);
        let sessions = [
            charge("2026-09", true, 30.0, Some(750.0)),
            charge("2026-09", true, 10.0, Some(0.0)),
            charge("2026-09", false, 20.0, Some(1200.0)),
            charge("2026-10", true, 12.0, Some(300.0)),
        ];
        let summary = summarise(&prices(), &km, &sessions);
        assert!(summary.configured);
        assert_eq!(summary.months.len(), 2);
        let september = &summary.months[0];
        assert_eq!(september.month, "2026-09");
        let p = &september.period;
        assert_eq!((p.km, p.home_kwh, p.away_kwh), (400.0, 40.0, 20.0));
        assert_eq!((p.home_cost, p.away_cost), (Some(7.5), Some(12.0)));
        assert_eq!(p.charging_cost, Some(19.5));
        // 400 km at 8 L/100km is 32 L, at $2.00.
        assert_eq!((p.petrol_litres, p.petrol_cost), (Some(32.0), Some(64.0)));
        assert_eq!(p.saved, Some(44.5));
        // $19.50 over 400 km is 4.875 c/km. Petrol is $64 over 400 km.
        assert_eq!(p.ev_cents_per_km, Some(4.88));
        assert_eq!(p.petrol_cents_per_km, Some(16.0));
        // The total covers both months.
        let t = &summary.total;
        assert_eq!((t.km, t.home_kwh, t.away_kwh), (500.0, 52.0, 20.0));
        assert_eq!(t.charging_cost, Some(22.5));
        assert_eq!(t.petrol_cost, Some(80.0));
        assert_eq!(t.saved, Some(57.5));
        assert_eq!(t.ev_cents_per_km, Some(4.5));
    }

    #[test]
    fn summary_without_a_petrol_price_has_no_money() {
        let km = BTreeMap::from([("2026-09".to_string(), 400.0)]);
        let sessions = [charge("2026-09", true, 30.0, Some(750.0))];
        let summary = summarise(&EvPrices::default(), &km, &sessions);
        assert!(!summary.configured);
        let total = &summary.total;
        assert_eq!((total.km, total.home_kwh), (400.0, 30.0));
        for money in [
            total.home_cost,
            total.away_cost,
            total.charging_cost,
            total.petrol_litres,
            total.petrol_cost,
            total.saved,
            total.ev_cents_per_km,
            total.petrol_cents_per_km,
        ] {
            assert_eq!(money, None);
        }
    }

    #[test]
    fn away_sessions_without_a_price_are_uncosted() {
        let km = BTreeMap::from([("2026-09".to_string(), 100.0)]);
        let sessions = [
            charge("2026-09", true, 10.0, Some(250.0)),
            charge("2026-09", false, 20.0, None),
        ];
        let p = summarise(&prices(), &km, &sessions).total;
        assert_eq!((p.away_kwh, p.away_cost), (20.0, None));
        // Only the home part is counted in the charging cost.
        assert_eq!(p.charging_cost, Some(2.5));
        // Without a tariff the home part cannot be costed either.
        let sessions = [charge("2026-09", true, 10.0, None)];
        let p = summarise(&prices(), &km, &sessions).total;
        assert_eq!((p.home_cost, p.charging_cost, p.saved), (None, None, None));
        assert_eq!(p.petrol_cost, Some(16.0));
    }

    #[test]
    fn no_km_means_no_cents_per_km() {
        let km = BTreeMap::new();
        let sessions = [charge("2026-09", true, 10.0, Some(250.0))];
        let summary = summarise(&prices(), &km, &sessions);
        // A month with a charge but no km still gets a row.
        assert_eq!(summary.months.len(), 1);
        let p = &summary.total;
        assert_eq!((p.ev_cents_per_km, p.petrol_cents_per_km), (None, None));
        assert_eq!(p.saved, Some(-2.5));
    }
}
