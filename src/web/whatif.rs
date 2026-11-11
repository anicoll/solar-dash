//! `GET /api/bill/amber-whatif`: what the home's own energy would have cost
//! on Amber, and what three habits (shifting car charging, charging the
//! battery from the grid when power is free, selling to price spikes) would
//! have saved. The model itself is in [`crate::whatif`]; this file loads each
//! day's data and shapes the JSON.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::api::{ApiError, ApiResult, day_bounds_or_error};
use super::bill::{MIN_PRICE_COVERAGE, compare_dates, hypothetical_tariff};
use super::costs::{SOURCE_BILL, SOURCE_GLOBIRD, billed_days, compare_days, round2};
use super::payback::Purchase;
use crate::db;
use crate::metric::Metric;
use crate::whatif::{
    ASSUMPTIONS, BatteryReading, CarSession, DEFAULT_RESERVE_PCT, DayInput, Levers,
    SPIKE_FEED_IN_CENTS, Savings, add_battery, build_slots, replay,
};

#[derive(Debug, Deserialize)]
pub struct WhatifParams {
    from: Option<String>,
    to: Option<String>,
    car: Option<String>,
    grid_charge: Option<String>,
    spikes: Option<String>,
}

/// `1`, `true` or `on` (any case) is on, anything else off.
fn flag(text: &str) -> bool {
    matches!(text.to_ascii_lowercase().as_str(), "1" | "true" | "on")
}

impl WhatifParams {
    /// The switches asked for. With none of the three given, all are on.
    fn levers(&self) -> Levers {
        if self.car.is_none() && self.grid_charge.is_none() && self.spikes.is_none() {
            return Levers::ALL;
        }
        let on = |value: &Option<String>| value.as_deref().is_some_and(flag);
        Levers {
            car: on(&self.car),
            grid_charge: on(&self.grid_charge),
            spikes: on(&self.spikes),
        }
    }
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

fn dollars(cents: f64) -> f64 {
    round2(cents / 100.0)
}

/// The same savings as a JSON object keyed by mix, in dollars.
fn mixes_json(mixes: &BTreeMap<String, f64>) -> Value {
    Value::Object(
        mixes
            .iter()
            .map(|(key, cents)| (key.clone(), json!(dollars(*cents))))
            .collect(),
    )
}

/// `GET /api/bill/amber-whatif?from=&to=&car=1&grid_charge=1&spikes=1`.
///
/// The range works like `compare-dynamic` (default: this bill so far; at most
/// 92 days; today never counts), and so does the 98% price coverage rule. The
/// three flags choose which switches `selected_*` figures include; with none
/// given all are on. `savings_dollars` always has every mix, so a page can
/// recompute for any switches without asking again.
///
/// ```text
/// {"configured":true,"from","to","levers":{"car","grid_charge","spikes"},
///  "battery_kwh","assumptions":[..],
///  "days":[{"date","globird_dollars","globird_source":"billed"|"estimate",
///           "amber_dollars","savings_dollars":{"car","grid_charge","spikes",
///           "car+grid_charge",..,"car+grid_charge+spikes"},
///           "selected_saving_dollars","car_sessions"}..],
///  "totals":{"days","skipped_days","globird_dollars","globird_billed_days",
///            "amber_dollars","difference_dollars","savings_dollars":{..},
///            "selected_saving_dollars","amber_with_switches_dollars"}}
/// ```
/// `difference_dollars` is Amber as used minus GloBird: negative = Amber is
/// cheaper. `globird_source` says whether the day's GloBird figure is a real
/// bill or our own estimate. Without `[amber]` and a plan it answers
/// `{"configured":false}`.
pub async fn amber_whatif(
    State(state): State<AppState>,
    Query(params): Query<WhatifParams>,
) -> ApiResult {
    let data = state.admin.get();
    let (Some(compare), Some(schedule)) = (&state.amber_compare, &data.tariff) else {
        return Ok(Json(json!({ "configured": false })));
    };
    let selected = params.levers();
    let (from, to, dates) = compare_dates(&state, params.from.as_deref(), params.to.as_deref())?;
    let hypothetical = hypothetical_tariff(compare)?;
    let compared = compare_days(&state, schedule, &hypothetical, &dates).await?;
    let billed: BTreeMap<NaiveDate, (&'static str, f64)> = billed_days(&state, schedule, &dates)
        .await?
        .into_iter()
        .map(|day| (day.date, (day.source, day.cost.actual_cents)))
        .collect();

    let mut rows = Vec::new();
    let mut skipped = 0;
    let mut sums = Totals::default();
    let mut battery_kwh_last = 0.0;
    for (day, hypo) in compared {
        if hypo.coverage < MIN_PRICE_COVERAGE {
            skipped += 1;
            continue;
        }
        let (start, end) = day_bounds_or_error(day.date, state.tz)?;
        let battery_kwh = battery_kwh_on(&data.purchases, day.date);
        battery_kwh_last = battery_kwh;
        let mut slots = build_slots(
            start,
            end,
            &hypo.intervals,
            &hypo.prices,
            compare.feed_in_paid_when_negative,
        );
        let readings = battery_readings(&state, start, end).await?;
        add_battery(&mut slots, start, battery_kwh, &readings);
        let reserve_pct = reserve_pct(&state, end).await?;
        let sessions: Vec<CarSession> = db::tm_home_sessions_started_in(&state.pool, start, end)
            .await?
            .into_iter()
            .map(|s| CarSession {
                start_ts: s.started_at,
                end_ts: s.ended_at,
                grid_kwh: s.grid_kwh,
            })
            .collect();
        let input = DayInput {
            start_ts: start,
            slots,
            battery_kwh,
            reserve_pct,
            sessions,
            spike_cents: SPIKE_FEED_IN_CENTS,
        };
        let mixes: BTreeMap<String, f64> = mix_savings(&input)
            .into_iter()
            .map(|(key, savings)| (key, savings.total_cents()))
            .collect();
        let selected_key = selected.key();
        let selected_cents = mixes.get(&selected_key).copied().unwrap_or(0.0);

        let (globird_cents, source) = match billed.get(&day.date) {
            Some(&(s, cents)) if s == SOURCE_GLOBIRD || s == SOURCE_BILL => (cents, "billed"),
            _ => (day.cost.actual_cents, "estimate"),
        };
        let amber_cents = hypo.priced.cost.actual_cents;
        sums.add(
            globird_cents,
            amber_cents,
            selected_cents,
            &mixes,
            source == "billed",
        );
        rows.push(json!({
            "date": day.date.to_string(),
            "globird_dollars": dollars(globird_cents),
            "globird_source": source,
            "amber_dollars": dollars(amber_cents),
            "savings_dollars": mixes_json(&mixes),
            "selected_saving_dollars": dollars(selected_cents),
            "car_sessions": input.sessions.len(),
        }));
    }
    let days = rows.len();
    Ok(Json(json!({
        "configured": true,
        "from": from.to_string(),
        "to": to.to_string(),
        "levers": {
            "car": selected.car,
            "grid_charge": selected.grid_charge,
            "spikes": selected.spikes,
        },
        "battery_kwh": round2(battery_kwh_last),
        "assumptions": ASSUMPTIONS,
        "days": rows,
        "totals": {
            "days": days,
            "skipped_days": skipped,
            "globird_dollars": dollars(sums.globird),
            "globird_billed_days": sums.billed_days,
            "amber_dollars": dollars(sums.amber),
            "difference_dollars": dollars(sums.amber - sums.globird),
            "savings_dollars": mixes_json(&sums.mixes),
            "selected_saving_dollars": dollars(sums.selected),
            "amber_with_switches_dollars": dollars(sums.amber - sums.selected),
        },
    })))
}

/// Running totals in cents, kept unrounded until the end.
#[derive(Default)]
struct Totals {
    globird: f64,
    amber: f64,
    selected: f64,
    billed_days: u32,
    mixes: BTreeMap<String, f64>,
}

impl Totals {
    fn add(
        &mut self,
        globird: f64,
        amber: f64,
        selected: f64,
        mixes: &BTreeMap<String, f64>,
        billed: bool,
    ) {
        self.globird += globird;
        self.amber += amber;
        self.selected += selected;
        self.billed_days += u32::from(billed);
        for (key, cents) in mixes {
            *self.mixes.entry(key.clone()).or_default() += cents;
        }
    }
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
