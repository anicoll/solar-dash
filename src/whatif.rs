//! The Amber "what if" model: replays one finished day of the house's real
//! energy on Amber's prices, then asks how much three habits would have saved.
//!
//! The three levers ("switches" on the Bill page):
//!
//! - **car**: each home charging session's grid energy is moved into the
//!   cheapest 5-minute slots Amber had while the car could have been charging.
//! - **grid charge**: when Amber's buy price was 0 c or below, the battery is
//!   filled from the grid, and that energy replaces the day's dearest later
//!   grid import.
//! - **spikes**: when Amber paid more than [`SPIKE_FEED_IN_CENTS`] to export,
//!   energy the battery really used for the house later on is sold instead,
//!   and the house buys that energy back later at that day's prices.
//!
//! The model is a deterministic greedy replay, one day at a time, with the
//! battery state carried **within the day only**. Nothing here touches the
//! database: the web layer builds a [`DayInput`] and this module returns
//! cents. Every assumption lives in a doc comment next to the code that makes
//! it, and [`ASSUMPTIONS`] is the same list in short sentences for the API.
//!
//! Money is in cents. The buy price is what a kWh of import costs (a negative
//! price pays you to import). The "earn" price is what a kWh of export pays
//! (positive means money in); the web layer flips Amber's raw sign for it.

use crate::cost::{DayPrices, Interval, span_at};

/// Seconds in one model slot. Amber settles prices every 5 minutes.
pub const SLOT_SECS: i64 = 300;
const SLOT_HOURS: f64 = SLOT_SECS as f64 / 3600.0;

/// The inverter's limit in kW, for charging and for discharging or exporting.
pub const INVERTER_KW: f64 = 8.0;
/// Share of energy lost putting it into the battery and getting it out again.
/// A typical figure for a home lithium battery; the real one is not measured.
pub const ROUND_TRIP_LOSS: f64 = 0.10;
/// The spike lever only sells when Amber pays more than this, in c/kWh.
pub const SPIKE_FEED_IN_CENTS: f64 = 50.0;
/// The battery reserve used when the inverter has not reported a setting, %.
pub const DEFAULT_RESERVE_PCT: f64 = 10.0;
/// A charging session can be moved within at least this many hours from its
/// start, even if it really ran for less: the car is assumed to stay plugged
/// in that long. (TeslaMate only records the time spent actually charging.)
pub const CAR_PLUGGED_MIN_HOURS: i64 = 6;
/// A battery reading older than this is not trusted for a slot.
const MAX_SOC_AGE_SECS: i64 = 30 * 60;

/// The assumptions behind every number, as short sentences for the API.
pub const ASSUMPTIONS: &[&str] = &[
    "Only days with Amber prices for at least 98% of the day are counted.",
    "Your real energy use is replayed on Amber's finished 5-minute prices, one day at a time. Supply and Amber's daily fee are not changed by any switch.",
    "Each switch is a greedy plan with hindsight: it picks the cheapest or dearest moments knowing the whole day, which is a best case, not a forecast.",
    "The battery state is carried within a day only. Every day starts from what the battery really held.",
    "Switches are applied in order: car, then grid charging, then spikes, each on the day as the previous one left it. A saving for any mix of switches is worked out on its own, so the numbers for a mix do not always add up from the single ones.",
    "Car: each home charging session's grid kWh moves to the cheapest 5-minute slots between when it started and 6 hours later (or when it really ended, if later), but never past midnight. The power is the session's average while charging, so it never goes above what the car really drew. Sessions not costed yet or with no grid figure are left alone.",
    "Grid charging: on a day with a buy price of 0 c or below, the battery is topped up from the grid, cheapest slots first, at up to 8 kW, only into the room it really had (capacity minus the charge it held then, never counting charge below the reserve). It is not filled with more than the day's later grid import could use.",
    "Grid charging: that energy replaces later grid import in the dearest slots, after charging has finished. 10% of it is lost in the battery.",
    "Spikes: when Amber pays more than 50 c/kWh for export, the battery sells at up to 8 kW (less what the home already exports), no lower than its reserve, but only as much as the battery really used for the house in the hours after.",
    "Spikes: the house then buys that energy back in the latest slots where the battery had been supplying it, at that day's actual buy prices. A sale is made only if it pays more than the buy-back costs.",
    "The battery's capacity is the sum of the batteries in your purchases. Without one, grid charging and spikes save nothing. The reserve is the higher of the inverter's reserve and minimum charge settings, or 10% if none was reported.",
    "Battery use comes from the inverter's live readings. A day without them gets no spike saving, and no grid-charging saving if the charge level is unknown.",
];

/// One 5-minute slot of a day.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Grid import in kWh.
    pub import_kwh: f64,
    /// Grid export in kWh.
    pub export_kwh: f64,
    /// House load in kWh. Kept to bound what the battery can have supplied.
    pub load_kwh: f64,
    /// Amber's buy price in c/kWh, `None` when no price covers the slot.
    pub buy: Option<f64>,
    /// What export earns in c/kWh (positive = money in), `None` when unpriced.
    pub earn: Option<f64>,
    /// kWh the battery really supplied to the house in this slot.
    pub battery_to_house_kwh: f64,
    /// Energy the battery held at the start of the slot in kWh, if known.
    pub soc_kwh: Option<f64>,
}

impl Slot {
    /// An empty, unpriced slot.
    pub fn empty() -> Self {
        Self {
            import_kwh: 0.0,
            export_kwh: 0.0,
            load_kwh: 0.0,
            buy: None,
            earn: None,
            battery_to_house_kwh: 0.0,
            soc_kwh: None,
        }
    }
}

/// A home charging session, already costed by TeslaMate history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarSession {
    /// Unix seconds the car started charging.
    pub start_ts: i64,
    /// Unix seconds the car stopped charging.
    pub end_ts: i64,
    /// The energy that came from the grid, in kWh.
    pub grid_kwh: f64,
}

/// One day to replay.
#[derive(Debug, Clone)]
pub struct DayInput {
    /// Unix seconds of local midnight.
    pub start_ts: i64,
    /// Slots from midnight on, 5 minutes each.
    pub slots: Vec<Slot>,
    /// Battery capacity in kWh, 0 when there is no battery.
    pub battery_kwh: f64,
    /// The battery reserve in %.
    pub reserve_pct: f64,
    pub sessions: Vec<CarSession>,
    /// The spike threshold in c/kWh. [`SPIKE_FEED_IN_CENTS`] in the app.
    pub spike_cents: f64,
}

/// Which switches are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Levers {
    pub car: bool,
    pub grid_charge: bool,
    pub spikes: bool,
}

impl Levers {
    pub const ALL: Levers = Levers {
        car: true,
        grid_charge: true,
        spikes: true,
    };

    /// The seven non-empty mixes, each with the key the API uses for it
    /// (`car`, `car+spikes`, ...).
    pub fn mixes() -> Vec<(String, Levers)> {
        let mut out = Vec::new();
        for bits in 1..8u8 {
            let levers = Levers {
                car: bits & 1 != 0,
                grid_charge: bits & 2 != 0,
                spikes: bits & 4 != 0,
            };
            out.push((levers.key(), levers));
        }
        out
    }

    /// `car+grid_charge+spikes` style key, in that fixed order.
    pub fn key(&self) -> String {
        let mut parts = Vec::new();
        if self.car {
            parts.push("car");
        }
        if self.grid_charge {
            parts.push("grid_charge");
        }
        if self.spikes {
            parts.push("spikes");
        }
        parts.join("+")
    }
}

/// Turns a day's intervals into 5-minute slots. Each interval's energy is
/// spread evenly over the slots it overlaps, so intervals of any length (live
/// samples or 5-minute counters) end up on one grid. A slot is priced at its
/// midpoint, the same rule the cost engine uses.
///
/// `paid_when_negative` says how to read a stored feed-in price: true means
/// negative = you are paid (Amber), so the earning is its negation.
pub fn build_slots(
    day_start: i64,
    day_end: i64,
    intervals: &[Interval],
    prices: &DayPrices,
    paid_when_negative: bool,
) -> Vec<Slot> {
    let count = ((day_end - day_start).max(0) + SLOT_SECS - 1) / SLOT_SECS;
    let mut slots = vec![Slot::empty(); count as usize];
    for iv in intervals {
        let len = iv.end_ts - iv.start_ts;
        if len <= 0 {
            continue;
        }
        let first = ((iv.start_ts.max(day_start) - day_start) / SLOT_SECS).max(0);
        let last = ((iv.end_ts.min(day_end) - day_start - 1) / SLOT_SECS).min(count - 1);
        for index in first..=last {
            let slot_start = day_start + index * SLOT_SECS;
            let overlap = iv.end_ts.min(slot_start + SLOT_SECS) - iv.start_ts.max(slot_start);
            if overlap <= 0 {
                continue;
            }
            let share = overlap as f64 / len as f64;
            let slot = &mut slots[index as usize];
            slot.import_kwh += iv.import_kwh * share;
            slot.export_kwh += iv.export_kwh * share;
            slot.load_kwh += iv.load_kwh * share;
        }
    }
    for (index, slot) in slots.iter_mut().enumerate() {
        let mid = day_start + index as i64 * SLOT_SECS + SLOT_SECS / 2;
        slot.buy = span_at(&prices.general, mid);
        slot.earn = span_at(&prices.feed_in, mid).map(|stored| {
            if paid_when_negative {
                0.0 - stored
            } else {
                stored
            }
        });
    }
    slots
}

/// What the battery was doing, from live readings (one per minute or so).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryReading {
    pub ts: i64,
    /// Positive = charging, negative = discharging, W.
    pub battery_w: Option<f64>,
    pub soc_pct: Option<f64>,
}

/// Puts the battery readings on the slots: how much the battery supplied to
/// the house, and what it held at the start of each slot.
///
/// The house's share of a discharge is the discharge minus what the home
/// exported in the slot (battery energy that went to the grid did not run the
/// house), and never more than the load. Charge level is the newest reading at
/// or before the slot's start, if it is under 30 minutes old.
pub fn add_battery(
    slots: &mut [Slot],
    day_start: i64,
    battery_kwh: f64,
    readings: &[BatteryReading],
) {
    let mut discharge = vec![0.0; slots.len()];
    for r in readings {
        let index = (r.ts - day_start).div_euclid(SLOT_SECS);
        let Some(w) = r.battery_w.filter(|w| *w < 0.0) else {
            continue;
        };
        // Each reading stands for the minute after it, the way the 1-minute
        // averages are built.
        if let Some(d) = usize::try_from(index)
            .ok()
            .and_then(|i| discharge.get_mut(i))
        {
            *d += -w / 1000.0 * (60.0 / 3600.0);
        }
    }
    for (slot, discharge_kwh) in slots.iter_mut().zip(discharge) {
        let to_house = (discharge_kwh - slot.export_kwh).max(0.0);
        slot.battery_to_house_kwh = to_house.min(slot.load_kwh);
    }
    let with_soc: Vec<(i64, f64)> = readings
        .iter()
        .filter_map(|r| Some((r.ts, r.soc_pct?)))
        .collect();
    for (index, slot) in slots.iter_mut().enumerate() {
        let at = day_start + index as i64 * SLOT_SECS;
        let known = with_soc.partition_point(|(ts, _)| *ts <= at);
        if let Some((ts, pct)) = known.checked_sub(1).map(|i| with_soc[i])
            && at - ts <= MAX_SOC_AGE_SECS
        {
            slot.soc_kwh = Some(pct / 100.0 * battery_kwh);
        }
    }
}

/// The energy cost in cents of what the lever changed, so a saving is cost
/// before minus cost after. Unpriced slots count nothing.
fn buy_cost(slot: &Slot, kwh: f64) -> f64 {
    slot.buy.map_or(0.0, |price| kwh * price)
}

/// The car lever. Returns the saving in cents and changes `slots`.
///
/// For each session: the grid energy is spread evenly over the slots it
/// really charged in (a steady charge at its average power) and costed there.
/// It is then put into the same number of cheapest priced slots between the
/// session's start and `max(its end, start + 6 h)`, clipped to the day. The
/// number of slots stays the same, so the shifted power equals the average
/// and never exceeds what the car drew. A session with an unpriced slot, or
/// that starts outside the day, is left alone.
fn shift_car(day: &mut DayInput) -> f64 {
    let mut saving = 0.0;
    let len = day.slots.len();
    let sessions = day.sessions.clone();
    for session in sessions {
        if session.grid_kwh <= 0.0 || session.end_ts <= session.start_ts {
            continue;
        }
        let first = (session.start_ts - day.start_ts).div_euclid(SLOT_SECS);
        let end_slot = (session.end_ts - day.start_ts + SLOT_SECS - 1).div_euclid(SLOT_SECS);
        if first < 0 || first as usize >= len {
            continue;
        }
        let first = first as usize;
        let real_end = (end_slot.max(first as i64 + 1) as usize).min(len);
        let plugged_end = (first + (CAR_PLUGGED_MIN_HOURS * 3600 / SLOT_SECS) as usize).min(len);
        let window_end = real_end.max(plugged_end);
        let actual: Vec<usize> = (first..real_end).collect();
        // The part of the session inside the day.
        let total_slots = ((session.end_ts - session.start_ts) as f64 / SLOT_SECS as f64)
            .ceil()
            .max(1.0);
        let grid_kwh = session.grid_kwh * (actual.len() as f64 / total_slots).min(1.0);
        let per_slot = grid_kwh / actual.len() as f64;
        if actual.iter().any(|&i| day.slots[i].buy.is_none()) {
            continue;
        }
        // Take the energy out of the slots it was drawn in...
        let mut removed = 0.0;
        let mut before = 0.0;
        for &i in &actual {
            let take = per_slot.min(day.slots[i].import_kwh);
            day.slots[i].import_kwh -= take;
            removed += take;
            before += buy_cost(&day.slots[i], take);
        }
        // ...and put it in the cheapest priced slots of the window.
        let mut cheapest: Vec<usize> = (first..window_end)
            .filter(|&i| day.slots[i].buy.is_some())
            .collect();
        cheapest.sort_by(|&a, &b| {
            day.slots[a]
                .buy
                .partial_cmp(&day.slots[b].buy)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        cheapest.truncate(actual.len());
        let each = removed / cheapest.len() as f64;
        let mut after = 0.0;
        for &i in &cheapest {
            day.slots[i].import_kwh += each;
            after += buy_cost(&day.slots[i], each);
        }
        saving += before - after;
    }
    saving
}

/// The grid-charge lever. Returns the saving in cents and changes `slots`.
///
/// Needs a battery, at least one slot priced 0 c or less, and a known charge
/// level at the first such slot. Room = capacity minus what the battery held
/// (counting at least the reserve as held). Charging fills the cheapest such
/// slots first at up to 8 kW, but never more than the import after that point
/// could use once 10% is lost. The stored energy then replaces import in the
/// dearest priced slots after the last charging slot, at up to 8 kW a slot.
///
/// Saving = avoided import cost minus the cost of charging (which is negative
/// when Amber paid for the import). Only the avoided energy suffers the loss.
fn grid_charge(day: &mut DayInput) -> f64 {
    if day.battery_kwh <= 0.0 {
        return 0.0;
    }
    let free: Vec<usize> = (0..day.slots.len())
        .filter(|&i| day.slots[i].buy.is_some_and(|price| price <= 0.0))
        .collect();
    let Some(&first) = free.first() else {
        return 0.0;
    };
    let Some(soc) = day.slots[first].soc_kwh else {
        return 0.0;
    };
    let reserve_kwh = day.reserve_pct / 100.0 * day.battery_kwh;
    let room = (day.battery_kwh - soc.max(reserve_kwh)).max(0.0);
    // No point storing more than the later import can use.
    let later_import: f64 = day.slots[first + 1..]
        .iter()
        .filter(|s| s.buy.is_some_and(|price| price > 0.0))
        .map(|s| s.import_kwh)
        .sum();
    let mut to_charge = room.min(later_import / (1.0 - ROUND_TRIP_LOSS));

    let mut order = free;
    order.sort_by(|&a, &b| {
        day.slots[a]
            .buy
            .partial_cmp(&day.slots[b].buy)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let per_slot_limit = INVERTER_KW * SLOT_HOURS;
    let mut charge_cost = 0.0;
    let mut charged = 0.0;
    let mut last_charge = first;
    for i in order {
        if to_charge <= 1e-9 {
            break;
        }
        let kwh = per_slot_limit.min(to_charge);
        day.slots[i].import_kwh += kwh;
        charge_cost += buy_cost(&day.slots[i], kwh);
        charged += kwh;
        to_charge -= kwh;
        last_charge = last_charge.max(i);
    }
    let mut delivered = charged * (1.0 - ROUND_TRIP_LOSS);
    let mut later: Vec<usize> = (last_charge + 1..day.slots.len())
        .filter(|&i| day.slots[i].buy.is_some_and(|price| price > 0.0))
        .collect();
    later.sort_by(|&a, &b| {
        day.slots[b]
            .buy
            .partial_cmp(&day.slots[a].buy)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let mut avoided_cost = 0.0;
    for i in later {
        if delivered <= 1e-9 {
            break;
        }
        let kwh = day.slots[i].import_kwh.min(per_slot_limit).min(delivered);
        if kwh <= 0.0 {
            continue;
        }
        day.slots[i].import_kwh -= kwh;
        avoided_cost += buy_cost(&day.slots[i], kwh);
        delivered -= kwh;
    }
    avoided_cost - charge_cost
}

/// The spike lever. Returns the saving in cents and changes `slots`.
///
/// Spike slots (Amber paying more than the threshold) are taken highest pay
/// first. In each, the battery sells as much as: the inverter has room for
/// (8 kW less the home's own export that slot), the battery holds above its
/// reserve (less what earlier spikes already sold), and the battery really
/// supplied to the house in later slots and has not been claimed yet. That
/// energy is bought back in the latest such slots first, since a battery that
/// sold energy runs dry at the end. A slot is skipped if buying back costs at
/// least what the sale pays.
///
/// Saving = export earned minus the import cost of buying back.
fn spikes(day: &mut DayInput) -> f64 {
    if day.battery_kwh <= 0.0 {
        return 0.0;
    }
    let reserve_kwh = day.reserve_pct / 100.0 * day.battery_kwh;
    let mut spike_slots: Vec<usize> = (0..day.slots.len())
        .filter(|&i| day.slots[i].earn.is_some_and(|e| e > day.spike_cents))
        .collect();
    spike_slots.sort_by(|&a, &b| {
        day.slots[b]
            .earn
            .partial_cmp(&day.slots[a].earn)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let mut pool: Vec<f64> = day.slots.iter().map(|s| s.battery_to_house_kwh).collect();
    let mut sold_total = 0.0;
    let mut saving = 0.0;
    for t in spike_slots {
        let (Some(soc), Some(earn)) = (day.slots[t].soc_kwh, day.slots[t].earn) else {
            continue;
        };
        let held = (soc - reserve_kwh - sold_total).max(0.0);
        let inverter_room = (INVERTER_KW * SLOT_HOURS - day.slots[t].export_kwh).max(0.0);
        let mut want = held.min(inverter_room);
        for i in (t + 1..day.slots.len()).rev() {
            if want <= 1e-9 {
                break;
            }
            let Some(buy) = day.slots[i].buy else {
                continue;
            };
            if pool[i] <= 1e-9 || earn - buy <= 0.0 {
                continue;
            }
            let kwh = pool[i].min(want);
            pool[i] -= kwh;
            want -= kwh;
            sold_total += kwh;
            day.slots[t].export_kwh += kwh;
            day.slots[i].import_kwh += kwh;
            saving += kwh * (earn - buy);
        }
    }
    saving
}

/// What the switches that were on saved on one day, in cents. Applied in a
/// fixed order (car, grid charge, spikes), each on the day the last one left.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Savings {
    pub car_cents: f64,
    pub grid_charge_cents: f64,
    pub spikes_cents: f64,
}

impl Savings {
    pub fn total_cents(&self) -> f64 {
        self.car_cents + self.grid_charge_cents + self.spikes_cents
    }
}

/// Replays `day` with the given switches and returns the savings.
pub fn replay(day: &DayInput, levers: Levers) -> Savings {
    let mut day = day.clone();
    let mut out = Savings::default();
    if levers.car {
        out.car_cents = shift_car(&mut day);
    }
    if levers.grid_charge {
        out.grid_charge_cents = grid_charge(&mut day);
    }
    if levers.spikes {
        out.spikes_cents = spikes(&mut day);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIDNIGHT: i64 = 1_000_000_000;
    const SLOTS: usize = 288;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// 24 hours of slots with a flat buy price of `buy` c and 0 c export.
    fn flat_day(buy: f64) -> DayInput {
        let slot = Slot {
            buy: Some(buy),
            earn: Some(0.0),
            ..Slot::empty()
        };
        DayInput {
            start_ts: MIDNIGHT,
            slots: vec![slot; SLOTS],
            battery_kwh: 25.6,
            reserve_pct: 10.0,
            sessions: Vec::new(),
            spike_cents: SPIKE_FEED_IN_CENTS,
        }
    }

    /// Slot index of a local hour.
    fn hour(h: usize) -> usize {
        h * 12
    }

    fn set_buy(day: &mut DayInput, from_h: usize, to_h: usize, cents: f64) {
        for s in &mut day.slots[hour(from_h)..hour(to_h)] {
            s.buy = Some(cents);
        }
    }

    #[test]
    fn mixes_have_stable_keys() {
        let keys: Vec<String> = Levers::mixes().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys.len(), 7);
        assert!(keys.contains(&"car".to_string()));
        assert!(keys.contains(&"car+grid_charge+spikes".to_string()));
        assert!(keys.contains(&"grid_charge+spikes".to_string()));
    }

    #[test]
    fn car_session_moves_to_the_cheapest_slots() {
        // 6 kWh over two hours from 18:00, dear at 60 c. 20:00 onward is 10 c.
        let mut day = flat_day(60.0);
        set_buy(&mut day, 20, 24, 10.0);
        day.sessions.push(CarSession {
            start_ts: MIDNIGHT + 18 * 3600,
            end_ts: MIDNIGHT + 20 * 3600,
            grid_kwh: 6.0,
        });
        for s in &mut day.slots[hour(18)..hour(20)] {
            s.import_kwh = 6.0 / 24.0;
        }
        let saving = replay(
            &day,
            Levers {
                car: true,
                ..Levers::default()
            },
        );
        // 6 kWh moved from 60 c to 10 c.
        assert!(close(saving.car_cents, 6.0 * 50.0), "{saving:?}");
        assert_eq!(saving.grid_charge_cents, 0.0);
    }

    #[test]
    fn car_window_ends_at_midnight_and_cannot_beat_a_flat_price() {
        let mut day = flat_day(30.0);
        day.sessions.push(CarSession {
            start_ts: MIDNIGHT + 22 * 3600,
            end_ts: MIDNIGHT + 23 * 3600,
            grid_kwh: 4.0,
        });
        for s in &mut day.slots[hour(22)..hour(23)] {
            s.import_kwh = 4.0 / 12.0;
        }
        let saving = replay(
            &day,
            Levers {
                car: true,
                ..Levers::default()
            },
        );
        assert!(close(saving.car_cents, 0.0));
        // The cheap hours after midnight are not reachable.
        set_buy(&mut day, 0, 22, 5.0);
        let saving = replay(
            &day,
            Levers {
                car: true,
                ..Levers::default()
            },
        );
        assert!(close(saving.car_cents, 0.0));
    }

    #[test]
    fn car_charging_in_a_negative_price_pays_you() {
        let mut day = flat_day(40.0);
        set_buy(&mut day, 22, 23, -10.0);
        day.sessions.push(CarSession {
            start_ts: MIDNIGHT + 20 * 3600,
            end_ts: MIDNIGHT + 21 * 3600,
            grid_kwh: 5.0,
        });
        for s in &mut day.slots[hour(20)..hour(21)] {
            s.import_kwh = 5.0 / 12.0;
        }
        let saving = replay(
            &day,
            Levers {
                car: true,
                ..Levers::default()
            },
        );
        // 12 slots in the window's 22:00 hour, all -10 c: 5 kWh at -10 vs 40.
        assert!(close(saving.car_cents, 5.0 * 50.0), "{saving:?}");
    }

    #[test]
    fn unpriced_session_slots_are_left_alone() {
        let mut day = flat_day(40.0);
        day.slots[hour(20)].buy = None;
        day.sessions.push(CarSession {
            start_ts: MIDNIGHT + 20 * 3600,
            end_ts: MIDNIGHT + 21 * 3600,
            grid_kwh: 5.0,
        });
        let saving = replay(
            &day,
            Levers {
                car: true,
                ..Levers::default()
            },
        );
        assert_eq!(saving.car_cents, 0.0);
    }

    /// A day with negative prices 11:00-12:00 and a dear evening import.
    fn negative_day() -> DayInput {
        let mut day = flat_day(30.0);
        set_buy(&mut day, 11, 12, -20.0);
        set_buy(&mut day, 17, 21, 60.0);
        for s in &mut day.slots[hour(17)..hour(21)] {
            s.import_kwh = 1.0 / 12.0 * 3.0; // 3 kW house load
        }
        // The battery held 10% (the reserve) at 11:00, so there is room.
        for s in &mut day.slots[hour(11)..hour(12)] {
            s.soc_kwh = Some(2.56);
        }
        day
    }

    #[test]
    fn grid_charge_fills_negative_slots_and_replaces_dear_import() {
        let day = negative_day();
        let saving = replay(
            &day,
            Levers {
                grid_charge: true,
                ..Levers::default()
            },
        );
        // Later import: 4 h x 3 kW = 12 kWh at 60 c. Charge room is 23.04 kWh
        // but only 12 / 0.9 = 13.33 kWh is worth storing. 12 slots x 0.6667 =
        // 8 kWh fit in the hour at 8 kW, paid -20 c; delivered 7.2 kWh.
        let charged = 8.0;
        let delivered = charged * 0.9;
        let expected = delivered * 60.0 + charged * 20.0;
        assert!(close(saving.grid_charge_cents, expected), "{saving:?}");
        assert!(saving.grid_charge_cents > 0.0);
    }

    #[test]
    fn grid_charge_needs_room_and_a_known_charge_level() {
        let mut day = negative_day();
        for s in &mut day.slots[hour(11)..hour(12)] {
            s.soc_kwh = Some(25.6); // already full
        }
        let full = replay(
            &day,
            Levers {
                grid_charge: true,
                ..Levers::default()
            },
        );
        assert_eq!(full.grid_charge_cents, 0.0);
        for s in &mut day.slots {
            s.soc_kwh = None;
        }
        let unknown = replay(
            &day,
            Levers {
                grid_charge: true,
                ..Levers::default()
            },
        );
        assert_eq!(unknown.grid_charge_cents, 0.0);
    }

    #[test]
    fn grid_charge_does_nothing_on_a_day_without_free_power() {
        let mut day = flat_day(30.0);
        for s in &mut day.slots {
            s.soc_kwh = Some(5.0);
            s.import_kwh = 0.5;
        }
        let saving = replay(
            &day,
            Levers {
                grid_charge: true,
                ..Levers::default()
            },
        );
        assert_eq!(saving.grid_charge_cents, 0.0);
    }

    #[test]
    fn grid_charge_without_a_battery_saves_nothing() {
        let mut day = negative_day();
        day.battery_kwh = 0.0;
        assert_eq!(replay(&day, Levers::ALL).total_cents(), 0.0);
    }

    /// A day with a 100 c spike at 18:00 and a battery that ran the house
    /// from 20:00 to 22:00 at 2 kW, on 40 c import.
    fn spike_day() -> DayInput {
        let mut day = flat_day(40.0);
        day.slots[hour(18)].earn = Some(100.0);
        for s in &mut day.slots[hour(18)..hour(22)] {
            s.soc_kwh = Some(12.0);
        }
        for s in &mut day.slots[hour(20)..hour(22)] {
            s.load_kwh = 2.0 / 12.0;
            s.battery_to_house_kwh = 2.0 / 12.0;
        }
        day
    }

    #[test]
    fn spikes_sell_what_the_battery_later_gave_the_house() {
        let day = spike_day();
        let saving = replay(
            &day,
            Levers {
                spikes: true,
                ..Levers::default()
            },
        );
        // One slot sells at most 8 kW * 5 min = 0.6667 kWh, and the 3 other
        // spike-free slots are not spikes. Sold at 100 c, bought back at 40 c.
        let sold = INVERTER_KW * SLOT_HOURS;
        assert!(close(saving.spikes_cents, sold * 60.0), "{saving:?}");
    }

    #[test]
    fn spikes_are_limited_by_what_the_battery_supplied_later() {
        let mut day = spike_day();
        for s in &mut day.slots[hour(18)..hour(19)] {
            s.earn = Some(100.0); // 12 spike slots, 8 kWh of room
        }
        let saving = replay(
            &day,
            Levers {
                spikes: true,
                ..Levers::default()
            },
        );
        // The battery only supplied 4 kWh later, so only 4 kWh is sold.
        assert!(close(saving.spikes_cents, 4.0 * 60.0), "{saving:?}");
    }

    #[test]
    fn spikes_respect_the_reserve_and_the_threshold() {
        let mut day = spike_day();
        for s in &mut day.slots[hour(18)..hour(22)] {
            s.soc_kwh = Some(2.56); // exactly the reserve
        }
        assert_eq!(
            replay(
                &day,
                Levers {
                    spikes: true,
                    ..Levers::default()
                }
            )
            .spikes_cents,
            0.0
        );
        let mut day = spike_day();
        day.slots[hour(18)].earn = Some(50.0); // not above the threshold
        assert_eq!(
            replay(
                &day,
                Levers {
                    spikes: true,
                    ..Levers::default()
                }
            )
            .spikes_cents,
            0.0
        );
    }

    #[test]
    fn spikes_skip_a_sale_that_costs_more_to_buy_back() {
        let mut day = spike_day();
        set_buy(&mut day, 20, 22, 120.0);
        let saving = replay(
            &day,
            Levers {
                spikes: true,
                ..Levers::default()
            },
        );
        assert_eq!(saving.spikes_cents, 0.0);
    }

    #[test]
    fn spike_buy_back_in_a_negative_slot_earns_extra() {
        let mut day = spike_day();
        set_buy(&mut day, 20, 22, -10.0);
        let saving = replay(
            &day,
            Levers {
                spikes: true,
                ..Levers::default()
            },
        );
        let sold = INVERTER_KW * SLOT_HOURS;
        assert!(close(saving.spikes_cents, sold * 110.0), "{saving:?}");
    }

    #[test]
    fn combined_is_the_sum_of_its_steps() {
        let mut day = negative_day();
        day.slots[hour(18)].earn = Some(90.0);
        for s in &mut day.slots[hour(18)..hour(21)] {
            s.soc_kwh = Some(15.0);
        }
        for s in &mut day.slots[hour(19)..hour(21)] {
            s.battery_to_house_kwh = 0.05;
            s.load_kwh = 0.25;
        }
        let all = replay(&day, Levers::ALL);
        assert!(close(
            all.total_cents(),
            all.car_cents + all.grid_charge_cents + all.spikes_cents
        ));
        assert!(all.grid_charge_cents > 0.0);
        assert!(all.spikes_cents > 0.0);
    }

    #[test]
    fn slots_are_built_from_intervals_and_prices() {
        use crate::cost::PriceSpan;
        let day_end = MIDNIGHT + 3600;
        // One 10-minute interval with 1 kWh in, 0.5 out.
        let iv = Interval {
            start_ts: MIDNIGHT,
            end_ts: MIDNIGHT + 600,
            import_kwh: 1.0,
            export_kwh: 0.5,
            load_kwh: 2.0,
        };
        let prices = DayPrices::new(
            vec![PriceSpan {
                start_ts: MIDNIGHT,
                end_ts: MIDNIGHT + 300,
                cents: 25.0,
            }],
            vec![PriceSpan {
                start_ts: MIDNIGHT,
                end_ts: MIDNIGHT + 3600,
                cents: -12.0,
            }],
        );
        let slots = build_slots(MIDNIGHT, day_end, &[iv], &prices, true);
        assert_eq!(slots.len(), 12);
        assert!(close(slots[0].import_kwh, 0.5));
        assert!(close(slots[1].import_kwh, 0.5));
        assert_eq!(slots[2].import_kwh, 0.0);
        assert_eq!(slots[0].buy, Some(25.0));
        assert_eq!(slots[1].buy, None);
        assert_eq!(slots[0].earn, Some(12.0));
    }

    #[test]
    fn battery_readings_become_house_supply_and_charge_level() {
        let mut slots = vec![Slot::empty(); 4];
        for s in &mut slots {
            s.load_kwh = 1.0;
        }
        slots[1].export_kwh = 0.1;
        // 6 kW discharge for the 5 minutes of slot 0 and slot 1.
        let mut readings = Vec::new();
        for minute in 0..10 {
            readings.push(BatteryReading {
                ts: MIDNIGHT + minute * 60,
                battery_w: Some(-6000.0),
                soc_pct: Some(50.0),
            });
        }
        add_battery(&mut slots, MIDNIGHT, 20.0, &readings);
        assert!(close(slots[0].battery_to_house_kwh, 0.5));
        assert!(close(slots[1].battery_to_house_kwh, 0.4));
        assert_eq!(slots[0].soc_kwh, Some(10.0));
        // A reading is trusted for 30 minutes, then the level is unknown.
        let mut long = vec![Slot::empty(); 12];
        add_battery(&mut long, MIDNIGHT, 20.0, &readings);
        assert_eq!(long[7].soc_kwh, Some(10.0));
        assert_eq!(long[8].soc_kwh, None);
    }
}
