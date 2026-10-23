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
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Timelike};
use chrono_tz::Tz;
use serde::Serialize;

use crate::bills::{self, PastBill};
use crate::cost::{Interval, TariffSchedule};
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

// ---------------------------------------------------------------------------
// Finished sessions, from TeslaMate's history
// ---------------------------------------------------------------------------

/// A power sample is held until the next one for at most this long. TeslaMate
/// writes about one a minute, so a longer gap is unknown time and adds nothing.
const MAX_POWER_HOLD_SECS: i64 = 300;

/// Bump this when the way a past session is costed changes, so sessions stored
/// by the old rules are worked out again.
pub const HISTORIC_COST_VERSION: u32 = 1;

/// What the sync needs to cost past sessions, built from the config the same
/// way the web side builds it.
#[derive(Debug, Clone)]
pub struct CostSetup {
    pub schedule: Arc<TariffSchedule>,
    pub bills: Arc<[PastBill]>,
    pub plant_id: i64,
}

/// The prices in cents per kWh at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Prices {
    /// What a kWh bought from the grid costs.
    pub import_cents: f64,
    /// What a kWh costs on the plan of the day, with no solar or battery.
    pub baseline_cents: f64,
    /// What a kWh sent to the grid earns.
    pub export_cents: f64,
}

/// A finished session worked out from the house's energy. Energy is in kWh,
/// money in cents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoricCost {
    /// All the energy the car took.
    pub kwh: f64,
    /// The part bought from the grid, including `uncovered_kwh`.
    pub grid_kwh: f64,
    /// The part from solar or the home battery. The cloud counters cannot tell
    /// those two apart, so they are one figure.
    pub self_kwh: f64,
    /// The part in time that no house interval covers. It is counted as grid,
    /// so this tells how much of the cost is an estimate.
    pub uncovered_kwh: f64,
    /// What the grid part cost.
    pub cost_cents: f64,
    /// What all of it would have cost with no solar or battery.
    pub baseline_cents: f64,
    /// What the solar and battery part would have earned if exported
    /// instead: money not earned, not money spent.
    pub feed_in_forgone_cents: f64,
}

/// A stretch of the car's energy: `kwh` drawn evenly over `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Draw {
    from: i64,
    to: i64,
    kwh: f64,
}

/// The car's energy over time from its power samples (`(ts, watts)`, oldest
/// first). Each sample is held until the next one, for at most
/// [`MAX_POWER_HOLD_SECS`], and clipped to `[start, end)`. If
/// `energy_used_kwh` is known and above 0, the profile is scaled to add up to
/// it, because the samples are rounded. With no usable samples the energy is
/// spread evenly over the whole session.
fn car_draws(
    power: &[(i64, f64)],
    start: i64,
    end: i64,
    energy_used_kwh: Option<f64>,
) -> Vec<Draw> {
    let mut draws: Vec<Draw> = power
        .iter()
        .enumerate()
        .filter_map(|(i, &(ts, watts))| {
            let next = power.get(i + 1).map_or(i64::MAX, |p| p.0);
            let (from, to) = (ts.max(start), (ts + MAX_POWER_HOLD_SECS).min(next).min(end));
            (to > from).then(|| Draw {
                from,
                to,
                kwh: watts.max(0.0) * (to - from) as f64 / WS_PER_KWH,
            })
        })
        .collect();
    let total: f64 = draws.iter().map(|d| d.kwh).sum();
    let energy = energy_used_kwh.filter(|kwh| *kwh > 0.0);
    match energy {
        Some(energy) if total > 0.0 => {
            for draw in &mut draws {
                draw.kwh *= energy / total;
            }
        }
        Some(energy) if end > start => {
            draws = vec![Draw {
                from: start,
                to: end,
                kwh: energy,
            }];
        }
        _ => {}
    }
    draws
}

/// The share (0 to 1) of `[start, end)` that `intervals` cover. They must be
/// sorted and must not overlap.
pub fn coverage(intervals: &[Interval], start: i64, end: i64) -> f64 {
    if end <= start {
        return 0.0;
    }
    let covered: i64 = intervals
        .iter()
        .map(|i| (i.end_ts.min(end) - i.start_ts.max(start)).max(0))
        .sum();
    covered as f64 / (end - start) as f64
}

/// One part of a draw: the time it lies in, its energy and the house interval
/// it falls in (`None` if no interval covers that time).
struct Piece {
    mid: i64,
    kwh: f64,
    interval: Option<usize>,
}

/// Cuts the draws at the boundaries of the intervals. Both are sorted, so one
/// pass is enough.
fn cut_pieces(draws: &[Draw], intervals: &[Interval]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    // The first interval that has not ended before the current draw began.
    let mut first = 0;
    for draw in draws {
        while first < intervals.len() && intervals[first].end_ts <= draw.from {
            first += 1;
        }
        let per_sec = draw.kwh / (draw.to - draw.from) as f64;
        let mut piece = |from: i64, to: i64, interval: Option<usize>| {
            if to > from {
                pieces.push(Piece {
                    mid: from + (to - from) / 2,
                    kwh: per_sec * (to - from) as f64,
                    interval,
                });
            }
        };
        // `at` is how far into the draw has been handed out.
        let mut at = draw.from;
        for (i, interval) in intervals.iter().enumerate().skip(first) {
            if interval.start_ts >= draw.to {
                break;
            }
            let (from, to) = (
                interval.start_ts.max(draw.from),
                interval.end_ts.min(draw.to),
            );
            piece(at, from, None);
            piece(from.max(at), to, Some(i));
            at = at.max(to);
        }
        piece(at, draw.to, None);
    }
    pieces
}

/// Works out a finished session `[start, end)` from the car's `power`
/// (`(ts, watts)`, oldest first) and the house `intervals` (sorted).
///
/// The car's energy over time comes from [`car_draws`]. Inside a house
/// interval the car is part of the house load, so it gets the load's grid
/// share (import over load); what is left comes from solar or the home
/// battery. If the car took more than the whole load in an interval (the
/// data does not agree), the excess counts as grid. Car energy in time no
/// interval covers counts as grid too and is reported as `uncovered_kwh`.
///
/// Every piece is priced at its own middle moment with `prices`.
pub fn historic_session_cost(
    power: &[(i64, f64)],
    start: i64,
    end: i64,
    energy_used_kwh: Option<f64>,
    intervals: &[Interval],
    prices: impl Fn(i64) -> Prices,
) -> HistoricCost {
    let draws = car_draws(power, start, end, energy_used_kwh);
    let pieces = cut_pieces(&draws, intervals);

    // The car's energy in each interval, to find its grid share.
    let mut car = vec![0.0; intervals.len()];
    for piece in &pieces {
        if let Some(i) = piece.interval {
            car[i] += piece.kwh;
        }
    }
    // The share of the car's energy in each interval that came from the grid.
    let grid_part: Vec<f64> = intervals
        .iter()
        .zip(&car)
        .map(|(interval, &car)| {
            if car <= 0.0 {
                return 0.0;
            }
            let load = interval.load_kwh;
            let share = if load > 0.0 {
                (interval.import_kwh / load).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let car_in = car.min(load.max(0.0));
            let grid = car_in * share + (car - car_in);
            grid / car
        })
        .collect();

    let mut cost = HistoricCost::default();
    for piece in &pieces {
        let price = prices(piece.mid);
        let grid = match piece.interval {
            Some(i) => piece.kwh * grid_part[i],
            None => {
                cost.uncovered_kwh += piece.kwh;
                piece.kwh
            }
        };
        let own = piece.kwh - grid;
        cost.kwh += piece.kwh;
        cost.grid_kwh += grid;
        cost.self_kwh += own;
        cost.cost_cents += grid * price.import_cents;
        cost.baseline_cents += piece.kwh * price.baseline_cents;
        cost.feed_in_forgone_cents += own * price.export_cents;
    }
    cost
}

/// The bill that covers a local day, if there is one.
fn bill_on(bills: &[PastBill], date: NaiveDate) -> Option<&PastBill> {
    bills.iter().find(|b| b.from <= date && date <= b.to)
}

/// The prices at a unix time, on the plan in effect that local day.
///
/// - Import: on a day inside a past bill, the bill's average import price,
///   because spot pricing (Amber) has no prices per interval. Otherwise the
///   plan's price at that time.
/// - Baseline: always the plan's import price at that time, the same
///   no-solar baseline Payback uses, bill days included.
/// - Export: always the plan's feed-in price. A bill's average export price is
///   skewed by the battery's spike exports, so the plan stands in for it.
///
/// A time far outside real data gets all zeros.
pub fn session_prices<'a>(
    schedule: &'a TariffSchedule,
    bills: &'a [PastBill],
    tz: Tz,
) -> impl Fn(i64) -> Prices + 'a {
    move |ts| {
        let Some((date, minute)) = local_time(tz, ts) else {
            return Prices::default();
        };
        let tariff = schedule.for_date(date);
        let baseline_cents = tariff.import_cents_at(minute);
        let import_cents = match bill_on(bills, date) {
            Some(bill) => bills::cents_per_kwh(bill.import_dollars, bill.import_kwh),
            None => baseline_cents,
        };
        Prices {
            import_cents,
            baseline_cents,
            export_cents: tariff.export_cents_at(minute),
        }
    }
}

/// What a stored session cost is tagged with: the costing rules, the time
/// zone, the plan on the day the session started and the bill covering that
/// day (its `from..to`, or "-"). A session whose tag differs from this is
/// worked out again.
pub fn historic_cost_key(
    schedule: &TariffSchedule,
    bills: &[PastBill],
    tz: Tz,
    started_at: i64,
) -> String {
    let date = local_time(tz, started_at).map(|(date, _)| date);
    let plan = date.map_or("-", |d| schedule.for_date(d).key());
    let bill = date
        .and_then(|d| bill_on(bills, d))
        .map_or("-".to_string(), |b| format!("{}..{}", b.from, b.to));
    format!("{HISTORIC_COST_VERSION}|{tz}|{plan}|{bill}")
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

    /// The ZeroHero-like plan: free from 11:00 to 14:00, 60.0 c from 15:00 to
    /// 23:00, 40 c otherwise, and 5 c for exporting.
    fn schedule() -> TariffSchedule {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            tariff: config::Tariff,
        }
        let w: Wrapper = toml::from_str(
            r#"
            [tariff]
            supply_cents_per_day = 100.0
            default_import_cents = 40.0
            import = [
              { from = "11:00", to = "14:00", cents = 0.0 },
              { from = "15:00", to = "23:00", cents = 60.0 },
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
        close(got.cost_cents, 7.4 * 60.0);
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
        close(got.cost_cents, 7.4 * 0.5 * 60.0);
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

    // --- Finished sessions ---

    fn interval(start_ts: i64, end_ts: i64, import: f64, export: f64, load: f64) -> Interval {
        Interval {
            start_ts,
            end_ts,
            import_kwh: import,
            export_kwh: export,
            load_kwh: load,
        }
    }

    /// The same prices at any time: 30 c import, 40 c baseline, 5 c export.
    fn flat(_ts: i64) -> Prices {
        Prices {
            import_cents: 30.0,
            baseline_cents: 40.0,
            export_cents: 5.0,
        }
    }

    /// 7.2 kW once a minute from `start` for `minutes`.
    fn steady_power(start: i64, minutes: i64) -> Vec<(i64, f64)> {
        (0..minutes).map(|m| (start + m * 60, 7200.0)).collect()
    }

    #[test]
    fn a_power_sample_is_held_until_the_next_but_not_past_five_minutes() {
        // 7.2 kW for 1 minute, then a 20 minute gap: only 5 minutes count.
        let draws = car_draws(&[(0, 7200.0), (60, 7200.0), (1260, 0.0)], 0, 3600, None);
        let kwh: Vec<f64> = draws.iter().map(|d| d.kwh).collect();
        close(kwh[0], 0.12);
        close(kwh[1], 0.6);
        close(kwh[2], 0.0);
        assert_eq!((draws[1].from, draws[1].to), (60, 360));
    }

    #[test]
    fn power_is_clipped_to_the_session() {
        let draws = car_draws(&[(0, 3600.0), (600, 3600.0)], 100, 700, None);
        // A sample is held from its own time, not from where the clip starts.
        assert_eq!((draws[0].from, draws[0].to), (100, 300));
        assert_eq!((draws[1].from, draws[1].to), (600, 700));
        // Samples wholly outside add nothing.
        assert!(car_draws(&[(5000, 3600.0)], 0, 100, None).is_empty());
    }

    #[test]
    fn the_profile_is_scaled_to_the_energy_used() {
        let power = steady_power(0, 60);
        let total = |energy| -> f64 {
            car_draws(&power, 0, 3600, energy)
                .iter()
                .map(|d| d.kwh)
                .sum()
        };
        close(total(None), 7.2);
        close(total(Some(6.0)), 6.0);
        // No energy to scale to: the profile stays as it is.
        close(total(Some(0.0)), 7.2);
    }

    #[test]
    fn without_power_the_energy_is_spread_evenly() {
        for power in [vec![], vec![(0, 0.0), (60, 0.0)]] {
            let draws = car_draws(&power, 1000, 4600, Some(5.0));
            assert_eq!(
                draws,
                vec![Draw {
                    from: 1000,
                    to: 4600,
                    kwh: 5.0
                }]
            );
        }
        assert!(car_draws(&[], 1000, 4600, None).is_empty());
        assert!(car_draws(&[], 1000, 1000, Some(5.0)).is_empty());
    }

    #[test]
    fn coverage_is_the_share_of_the_session_inside_intervals() {
        let intervals = [
            interval(0, 1000, 0.0, 0.0, 0.0),
            interval(1000, 1500, 0.0, 0.0, 0.0),
            interval(3000, 4000, 0.0, 0.0, 0.0),
        ];
        close(coverage(&intervals, 500, 2500), 1000.0 / 2000.0);
        close(coverage(&intervals, 0, 1500), 1.0);
        close(coverage(&intervals, 5000, 6000), 0.0);
        close(coverage(&[], 0, 100), 0.0);
        close(coverage(&intervals, 100, 100), 0.0);
    }

    #[test]
    fn the_car_gets_the_loads_grid_share_in_an_interval() {
        // 1 kWh of the house's 4 kWh load came from the grid: a quarter.
        let intervals = [interval(0, 3600, 1.0, 0.0, 4.0)];
        let power = steady_power(0, 60);
        let got = historic_session_cost(&power, 0, 3600, Some(2.0), &intervals, flat);
        close(got.kwh, 2.0);
        close(got.grid_kwh, 0.5);
        close(got.self_kwh, 1.5);
        close(got.uncovered_kwh, 0.0);
        close(got.cost_cents, 0.5 * 30.0);
        close(got.baseline_cents, 2.0 * 40.0);
        close(got.feed_in_forgone_cents, 1.5 * 5.0);
    }

    #[test]
    fn all_solar_costs_nothing_and_forgoes_the_feed_in() {
        let intervals = [interval(0, 3600, 0.0, 3.0, 5.0)];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(4.0), &intervals, flat);
        close(got.grid_kwh, 0.0);
        close(got.self_kwh, 4.0);
        close(got.cost_cents, 0.0);
        close(got.feed_in_forgone_cents, 20.0);
        close(got.baseline_cents, 160.0);
    }

    #[test]
    fn a_car_bigger_than_the_load_counts_the_excess_as_grid() {
        // The house load says 1 kWh, all from the grid... or half of it.
        let intervals = [interval(0, 3600, 0.5, 0.0, 1.0)];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(3.0), &intervals, flat);
        // 1 kWh fits in the load, half of it grid; the other 2 kWh are excess.
        close(got.grid_kwh, 0.5 + 2.0);
        close(got.self_kwh, 0.5);
    }

    #[test]
    fn an_interval_with_no_load_counts_as_grid() {
        let intervals = [interval(0, 3600, 0.0, 0.0, 0.0)];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(3.0), &intervals, flat);
        close(got.grid_kwh, 3.0);
        close(got.self_kwh, 0.0);
        close(got.uncovered_kwh, 0.0);
    }

    #[test]
    fn a_grid_share_above_one_is_capped() {
        // Import is more than load (meters disagree): all grid, never more.
        let intervals = [interval(0, 3600, 5.0, 0.0, 2.0)];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(2.0), &intervals, flat);
        close(got.grid_kwh, 2.0);
        close(got.self_kwh, 0.0);
    }

    #[test]
    fn time_without_intervals_is_uncovered_and_counted_as_grid() {
        // The house data covers only the first half hour.
        let intervals = [interval(0, 1800, 0.0, 0.0, 10.0)];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(4.0), &intervals, flat);
        close(got.kwh, 4.0);
        close(got.uncovered_kwh, 2.0);
        // The covered half is all solar; the uncovered half is grid.
        close(got.grid_kwh, 2.0);
        close(got.self_kwh, 2.0);
        close(got.cost_cents, 60.0);
        // No interval at all: everything is uncovered.
        let none = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(4.0), &[], flat);
        close(none.uncovered_kwh, 4.0);
        close(none.grid_kwh, 4.0);
        close(none.cost_cents, 120.0);
    }

    #[test]
    fn the_car_energy_in_an_interval_is_shared_across_its_pieces() {
        // Two power samples fall in one interval; the load cap is on the sum.
        let intervals = [interval(0, 3600, 0.0, 0.0, 1.0)];
        let power = [(0, 7200.0), (1800, 7200.0)];
        let got = historic_session_cost(&power, 0, 3600, Some(4.0), &intervals, flat);
        // 1 kWh of the 4 is in the load (all solar); 3 kWh are excess.
        close(got.self_kwh, 1.0);
        close(got.grid_kwh, 3.0);
    }

    #[test]
    fn each_piece_is_priced_at_its_own_time() {
        // Cheap before t=1800, dear after.
        let prices = |ts: i64| Prices {
            import_cents: if ts < 1800 { 10.0 } else { 50.0 },
            baseline_cents: if ts < 1800 { 10.0 } else { 50.0 },
            export_cents: 0.0,
        };
        // One interval spans the change; the power samples are hourly in
        // effect (held 5 minutes each), so cut at the interval edges too.
        let intervals = [
            interval(0, 1800, 4.0, 0.0, 4.0),
            interval(1800, 3600, 4.0, 0.0, 4.0),
        ];
        let power = [(0, 3600.0), (1800, 3600.0)];
        let got = historic_session_cost(&power, 0, 3600, Some(2.0), &intervals, prices);
        // Half of the 2 kWh at each price, all from the grid.
        close(got.cost_cents, 1.0 * 10.0 + 1.0 * 50.0);
        close(got.baseline_cents, 60.0);
    }

    #[test]
    fn a_draw_spanning_several_intervals_is_cut_at_their_edges() {
        let draws = [Draw {
            from: 100,
            to: 400,
            kwh: 3.0,
        }];
        let intervals = [
            interval(0, 200, 0.0, 0.0, 1.0),
            interval(200, 300, 0.0, 0.0, 1.0),
            interval(350, 500, 0.0, 0.0, 1.0),
        ];
        let pieces = cut_pieces(&draws, &intervals);
        let got: Vec<(Option<usize>, i64, f64)> =
            pieces.iter().map(|p| (p.interval, p.mid, p.kwh)).collect();
        assert_eq!(
            got,
            vec![
                (Some(0), 150, 1.0),
                (Some(1), 250, 1.0),
                (None, 325, 0.5),
                (Some(2), 375, 0.5),
            ]
        );
    }

    #[test]
    fn nothing_to_cost_gives_zeros() {
        let got = historic_session_cost(&[], 0, 3600, None, &[], flat);
        assert_eq!(got, HistoricCost::default());
    }

    #[test]
    fn energy_adds_up_whatever_the_intervals_are() {
        let intervals = [
            interval(500, 1500, 0.3, 0.1, 2.0),
            interval(1500, 2500, 1.9, 0.0, 2.0),
        ];
        let got = historic_session_cost(&steady_power(0, 60), 0, 3600, Some(5.5), &intervals, flat);
        close(got.kwh, 5.5);
        close(got.grid_kwh + got.self_kwh, 5.5);
        assert!(got.uncovered_kwh <= got.grid_kwh + 1e-9);
    }

    fn bill(from: &str, to: &str) -> PastBill {
        PastBill {
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            bill_total: 0.0,
            excluded: 0.0,
            import_kwh: 200.0,
            import_dollars: 50.0,
            export_kwh: 100.0,
            export_dollars: 40.0,
            supply_dollars: 0.0,
            note: String::new(),
        }
    }

    #[test]
    fn prices_follow_the_plan_at_that_time() {
        let schedule = schedule();
        let prices = session_prices(&schedule, &[], ADELAIDE);
        let peak = prices(at(18, 0));
        assert_eq!(
            peak,
            Prices {
                import_cents: 60.0,
                baseline_cents: 60.0,
                export_cents: 5.0
            }
        );
        assert_eq!(prices(at(12, 0)).import_cents, 0.0);
        assert_eq!(prices(at(2, 0)).import_cents, 40.0);
    }

    #[test]
    fn a_bill_day_imports_at_the_bills_average_but_keeps_the_plan_baseline() {
        let schedule = schedule();
        let bills = [bill("2026-09-10", "2026-09-15")];
        let prices = session_prices(&schedule, &bills, ADELAIDE);
        let got = prices(at(18, 0));
        // $50 over 200 kWh.
        close(got.import_cents, 25.0);
        close(got.baseline_cents, 60.0);
        close(got.export_cents, 5.0);
        // The free window of the plan is no longer free on a bill day.
        close(prices(at(12, 0)).import_cents, 25.0);
        close(prices(at(12, 0)).baseline_cents, 0.0);
        // The day after the bill is on the plan again.
        let next = at(18, 0) + 86_400;
        close(prices(next).import_cents, 60.0);
    }

    #[test]
    fn the_cost_key_follows_the_rules_zone_plan_and_bill() {
        let schedule = schedule();
        let start = at(18, 0);
        let plain = historic_cost_key(&schedule, &[], ADELAIDE, start);
        assert!(plain.starts_with(&format!("{HISTORIC_COST_VERSION}|Australia/Adelaide|")));
        assert!(plain.ends_with("|-"));
        assert_eq!(plain, historic_cost_key(&schedule, &[], ADELAIDE, start));
        let billed = historic_cost_key(
            &schedule,
            &[bill("2026-09-10", "2026-09-15")],
            ADELAIDE,
            start,
        );
        assert!(billed.ends_with("|2026-09-10..2026-09-15"));
        assert_ne!(plain, billed);
        // A bill elsewhere does not matter.
        let elsewhere = historic_cost_key(
            &schedule,
            &[bill("2026-08-01", "2026-08-31")],
            ADELAIDE,
            start,
        );
        assert_eq!(plain, elsewhere);
        // Another zone, or a changed plan, changes it.
        assert_ne!(
            plain,
            historic_cost_key(&schedule, &[], chrono_tz::Australia::Perth, start)
        );
        let other = {
            #[derive(serde::Deserialize)]
            struct Wrapper {
                tariff: config::Tariff,
            }
            let w: Wrapper = toml::from_str(
                "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 41.0\n\
                 default_export_cents = 5.0\n",
            )
            .unwrap();
            TariffSchedule::from_config(&w.tariff, &[]).unwrap()
        };
        assert_ne!(plain, historic_cost_key(&other, &[], ADELAIDE, start));
    }
}
