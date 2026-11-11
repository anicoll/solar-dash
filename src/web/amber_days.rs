//! One finished day on the made-up Amber plan: what the Amber comparison
//! (`/api/bill/compare-dynamic`) and the what-if (`/api/bill/amber-whatif`)
//! both need. Working a day out means rebuilding its energy intervals from the
//! stored samples, pricing them twice and replaying the what-if switches, which
//! takes tens of milliseconds a day. A finished day never changes, so the
//! result is kept in the `daily_cache` table (see [`super::day_cache`]) and only
//! days that are not stored yet are worked out.

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::api::{ApiError, day_bounds_or_error, first_data_ts};
use super::bill::{MIN_PRICE_COVERAGE, hypothetical_tariff};
use super::costs::{HypotheticalDay, cache_key, compare_days, consecutive_runs};
use super::day_cache;
use super::payback::Purchase;
use crate::amber::store as amber_store;
use crate::config::AmberCompare;
use crate::cost::{Tariff, TariffSchedule};
use crate::db;
use crate::metric::Metric;
use crate::whatif::{
    BatteryReading, CarSession, DEFAULT_RESERVE_PCT, DayInput, Levers, SPIKE_FEED_IN_CENTS,
    Savings, add_battery, build_slots, replay,
};

/// The `daily_cache` kind these days are stored under.
const KIND: &str = "amber_whatif";

/// Bump it whenever a change to the costing, the what-if model or this struct
/// would make an old stored day wrong; every stored day is then worked out
/// again.
const CACHE_VERSION: u32 = 1;

/// One finished day, in cents, as both Amber endpoints use it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct AmberDay {
    pub date: NaiveDate,
    /// The day on the plan in force then.
    pub current_cents: f64,
    /// The share of the day (0 to 1) that has a stored import price.
    pub coverage: f64,
    /// The same energy on the made-up Amber plan.
    pub amber_cents: f64,
    /// Minutes of the Amber cost that fell back to the plan's own windows.
    pub fallback_minutes: u32,
    /// What each mix of what-if switches would have saved. Empty on a day
    /// with too few prices, which the what-if skips anyway.
    pub mixes: BTreeMap<String, f64>,
    /// Home charging sessions that started on the day.
    pub car_sessions: usize,
    /// The home battery's capacity on the day, in kWh.
    pub battery_kwh: f64,
}

/// What the what-if needs from the day besides its energy and prices.
struct DayExtras {
    battery_kwh: f64,
    reserve_pct: f64,
    sessions: Vec<CarSession>,
}

/// The battery capacity in kWh on `date`: the sum of the `battery_kwh` of the
/// purchases made on or before it. 0 when there is none.
fn battery_kwh_on(purchases: &[Purchase], date: NaiveDate) -> f64 {
    purchases
        .iter()
        .filter(|p| p.date <= date)
        .filter_map(|p| p.battery_kwh)
        .sum()
}

/// Savings for every mix of switches, in cents.
fn mix_savings(day: &DayInput) -> BTreeMap<String, Savings> {
    Levers::mixes()
        .into_iter()
        .map(|(key, levers)| (key, replay(day, levers)))
        .collect()
}

/// The battery readings of a day: live readings (1-minute averages) when the
/// inverter was recorded, else the cloud's charge level alone.
async fn battery_readings(
    state: &AppState,
    start: i64,
    end: i64,
) -> Result<Vec<BatteryReading>, ApiError> {
    let live = db::live_samples_in_range(&state.pool, start, end).await?;
    if !live.is_empty() {
        return Ok(live
            .into_iter()
            .map(|s| BatteryReading {
                ts: s.ts,
                battery_w: s.battery_w,
                soc_pct: s.soc_pct,
            })
            .collect());
    }
    let cloud =
        db::readings_in_range(&state.pool, state.plant_id, Metric::SocPct, start, end).await?;
    Ok(cloud
        .into_iter()
        .map(|(ts, pct)| BatteryReading {
            ts,
            battery_w: None,
            soc_pct: Some(pct),
        })
        .collect())
}

/// The battery reserve in force at the end of the day: the higher of the
/// inverter's backup reserve and minimum charge settings, else 10%.
async fn reserve_pct(state: &AppState, day_end: i64) -> Result<f64, ApiError> {
    let settings = db::inverter_settings_before(&state.pool, day_end).await?;
    Ok(settings
        .and_then(|s| {
            let reserve = s.backup_reserve_pct.map(f64::from);
            [reserve, s.min_soc_pct]
                .into_iter()
                .flatten()
                .reduce(f64::max)
        })
        .unwrap_or(DEFAULT_RESERVE_PCT))
}

/// The cheap inputs of one day, read per day (a few indexed rows each).
async fn day_extras(
    state: &AppState,
    purchases: &[Purchase],
    date: NaiveDate,
    start: i64,
    end: i64,
) -> Result<DayExtras, ApiError> {
    let sessions = db::tm_home_sessions_started_in(&state.pool, start, end)
        .await?
        .into_iter()
        .map(|s| CarSession {
            start_ts: s.started_at,
            end_ts: s.ended_at,
            grid_kwh: s.grid_kwh,
        })
        .collect();
    Ok(DayExtras {
        battery_kwh: battery_kwh_on(purchases, date),
        reserve_pct: reserve_pct(state, end).await?,
        sessions,
    })
}

/// What a stored day was worked out from. Everything the figures depend on is
/// in it, so changing any of it makes the day be worked out again: the plan in
/// force and the Amber plan, the day's price rows (finished prices only, as the
/// costing uses), the source rules, and the what-if's own inputs.
async fn day_key(
    state: &AppState,
    tariff: &Tariff,
    hypothetical: &Tariff,
    compare: &AmberCompare,
    extras: &DayExtras,
    start: i64,
    end: i64,
) -> Result<String, ApiError> {
    // Finished days only ever use `actual` prices, so `include_current` is false.
    let mut prices = String::new();
    for source in [tariff.dynamic_source(), hypothetical.dynamic_source()]
        .into_iter()
        .flatten()
    {
        let fingerprint =
            amber_store::day_prices_fingerprint(&state.pool, source, start, end, false).await?;
        prices.push_str(&format!("{source}={fingerprint};"));
    }
    let sessions: Vec<(i64, i64, u64)> = extras
        .sessions
        .iter()
        .map(|s| (s.start_ts, s.end_ts, s.grid_kwh.to_bits()))
        .collect();
    Ok(format!(
        "{CACHE_VERSION}|{}|{}|{prices}|{:?}|paid_negative={}|spike={}|battery={}|reserve={}|{sessions:?}",
        cache_key(state.tz, tariff),
        hypothetical.key(),
        state.rules,
        compare.feed_in_paid_when_negative,
        SPIKE_FEED_IN_CENTS.to_bits(),
        extras.battery_kwh.to_bits(),
        extras.reserve_pct.to_bits(),
    ))
}

/// Works out one day's [`AmberDay`] from the energy and prices it was costed from.
async fn work_out(
    state: &AppState,
    compare: &AmberCompare,
    day: &super::costs::CostedDay,
    hypo: &HypotheticalDay,
    extras: &DayExtras,
) -> Result<AmberDay, ApiError> {
    let mut mixes = BTreeMap::new();
    if hypo.coverage >= MIN_PRICE_COVERAGE {
        let (start, end) = day_bounds_or_error(day.date, state.tz)?;
        let mut slots = build_slots(
            start,
            end,
            &hypo.intervals,
            &hypo.prices,
            compare.feed_in_paid_when_negative,
        );
        let readings = battery_readings(state, start, end).await?;
        add_battery(&mut slots, start, extras.battery_kwh, &readings);
        let input = DayInput {
            start_ts: start,
            slots,
            battery_kwh: extras.battery_kwh,
            reserve_pct: extras.reserve_pct,
            sessions: extras.sessions.clone(),
            spike_cents: SPIKE_FEED_IN_CENTS,
        };
        mixes = mix_savings(&input)
            .into_iter()
            .map(|(key, savings)| (key, savings.total_cents()))
            .collect();
    }
    Ok(AmberDay {
        date: day.date,
        current_cents: day.cost.actual_cents,
        coverage: hypo.coverage,
        amber_cents: hypo.priced.cost.actual_cents,
        fallback_minutes: hypo.priced.fallback_minutes,
        mixes,
        car_sessions: extras.sessions.len(),
        battery_kwh: extras.battery_kwh,
    })
}

/// Every complete day in `dates` (sorted, consecutive; today never among them)
/// on the plan in force and on the Amber plan, oldest first. Days that are
/// complete and already stored come from `daily_cache`; the rest are worked out
/// together (as [`compare_days`] does) and stored.
pub(super) async fn amber_days(
    state: &AppState,
    schedule: &TariffSchedule,
    compare: &AmberCompare,
    purchases: &[Purchase],
    dates: &[NaiveDate],
) -> Result<Vec<AmberDay>, ApiError> {
    let hypothetical = hypothetical_tariff(compare)?;
    // A day that ends before the first reading has no energy, so it has no
    // figures and needs no query at all.
    let first_data = first_data_ts(state).await?;
    let mut dates_with_data = Vec::with_capacity(dates.len());
    for &date in dates {
        let (_, end) = day_bounds_or_error(date, state.tz)?;
        if first_data.is_some_and(|first| end > first) {
            dates_with_data.push(date);
        }
    }
    let dates = &dates_with_data[..];
    // The day's key and extras, which are cheap queries; the energy is not read yet.
    let mut keys = Vec::with_capacity(dates.len());
    let mut extras: HashMap<NaiveDate, DayExtras> = HashMap::new();
    for &date in dates {
        let (start, end) = day_bounds_or_error(date, state.tz)?;
        let day_extras = day_extras(state, purchases, date, start, end).await?;
        let key = day_key(
            state,
            schedule.for_date(date),
            &hypothetical,
            compare,
            &day_extras,
            start,
            end,
        )
        .await?;
        keys.push((date, key));
        extras.insert(date, day_extras);
    }
    let mut stored: HashMap<NaiveDate, AmberDay> =
        day_cache::load(&state.pool, KIND, &keys).await?;

    let missing: Vec<NaiveDate> = dates
        .iter()
        .copied()
        .filter(|date| !stored.contains_key(date))
        .collect();
    let mut to_store = Vec::new();
    for run in consecutive_runs(&missing) {
        for (day, hypo) in compare_days(state, schedule, &hypothetical, run).await? {
            let day_extras = &extras[&day.date];
            let worked = work_out(state, compare, &day, &hypo, day_extras).await?;
            to_store.push(worked);
        }
    }
    let key_of: HashMap<NaiveDate, &String> = keys.iter().map(|(d, k)| (*d, k)).collect();
    let rows: Vec<(NaiveDate, String, &AmberDay)> = to_store
        .iter()
        .map(|day| (day.date, key_of[&day.date].clone(), day))
        .collect();
    day_cache::save(&state.pool, KIND, &rows).await?;
    stored.extend(to_store.into_iter().map(|day| (day.date, day)));

    // Back in date order, which a `HashMap` does not keep.
    Ok(dates.iter().filter_map(|d| stored.remove(d)).collect())
}
