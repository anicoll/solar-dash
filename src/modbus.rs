//! Reading live power straight from the inverter over Modbus TCP.
//!
//! The inverter is a Sungrow SH8.0RS, reached through its WiNet-S network
//! dongle. The decoding of register values is done by small pure functions
//! (no network) so they can be unit tested with real readings. [`Reader`]
//! does the network part.
//!
//! The dongle is known to stall or drop connections, so every request has a
//! timeout and any error throws the connection away; the next read starts
//! with a fresh one.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::net::lookup_host;
use tokio::time::{sleep, timeout};
use tokio_modbus::client::{Context as ModbusContext, Reader as _, tcp};
use tokio_modbus::{ExceptionCode, Slave};
use tracing::{debug, info, warn};

use crate::config::Modbus;
use crate::db;

/// How long one request (or the connection attempt) may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// Pause between requests, to go easy on the dongle.
const REQUEST_PAUSE: Duration = Duration::from_millis(50);

// Register addresses. These are 0-based and are read with function 04
// (read input registers). Values wider than 16 bits use two registers,
// low word first.
/// PV total DC power, W, u32.
const PV_POWER_REG: u16 = 5016;
/// Battery power, W, i32. The inverter sends negative = charging.
const BATTERY_POWER_REG: u16 = 5213;
/// The block 13007..=13022 holds load power (13007, i32), grid export power
/// (13009, i32) and battery SOC (13022, u16, 0.1 %). One read of the block is
/// cheaper than three separate reads.
const BLOCK_START: u16 = 13007;
const BLOCK_LEN: u16 = 16;

/// The block 13036..=13046 holds the lifetime grid import energy (13036, u32,
/// 0.1 kWh) and the lifetime grid export energy (13045, u32, 0.1 kWh).
const METER_START: u16 = 13036;
const METER_LEN: u16 = 11;
/// The grid energy counters count in steps of 0.1 kWh.
const METER_UNIT_WH: f64 = 100.0;

// Control settings. These are read with function 03 (read holding registers),
// except the running state, which is an input register. Every one is read-only
// here: nothing in this program ever writes to the inverter.
/// EMS mode, forced command and forced power are three registers in a row.
const EMS_START: u16 = 13049;
const EMS_LEN: u16 = 3;
/// Max SOC then min SOC, 0.1 % each.
const SOC_LIMITS_START: u16 = 13057;
const SOC_LIMITS_LEN: u16 = 2;
/// Export power limit, W.
const EXPORT_LIMIT_REG: u16 = 13073;
/// Export limit mode: [`EXPORT_LIMIT_ON`] or [`EXPORT_LIMIT_OFF`].
const EXPORT_MODE_REG: u16 = 13086;
/// Reserved SOC for backup, whole %.
const BACKUP_RESERVE_REG: u16 = 13099;
/// Battery max charge power then max discharge power, units of 10 W.
const BATTERY_LIMITS_START: u16 = 33046;
const BATTERY_LIMITS_LEN: u16 = 2;
/// Running state (input register).
const RUNNING_STATE_REG: u16 = 12999;

const EXPORT_LIMIT_ON: u16 = 0xAA;
const EXPORT_LIMIT_OFF: u16 = 0x55;
/// The battery power limit registers count in tens of watts.
const BATTERY_LIMIT_UNIT_W: u32 = 10;

/// The inverter sends this for a 32-bit value it cannot measure right now.
const UNAVAILABLE: u32 = 0x7FFF_FFFF;
/// An unsigned register (or pair) reads all ones when the inverter has no
/// value for it; the PV register is unsigned, so this is its "unavailable".
const UNAVAILABLE_U16: u16 = 0xFFFF;
const UNAVAILABLE_U32_ALL_ONES: u32 = 0xFFFF_FFFF;

// Sanity limits. A reading outside these is a glitch, not a measurement, and
// is stored as `None` so it cannot reach the daily kWh and cost totals.
/// Highest believable PV power, W: 7 kW of panels on an 8 kW inverter, so
/// 20 kW is far more than it can ever produce, yet far below a garbage u32.
const PV_MAX_W: f64 = 20_000.0;
/// Highest believable battery power, W (either direction). The 8 kW inverter
/// limits it well below this; the rest is headroom.
const BATTERY_MAX_W: f64 = 30_000.0;
/// Highest believable grid power, W (either direction). A single-phase 63 A
/// supply is about 15 kW; 30 kW leaves double that as headroom.
const GRID_MAX_W: f64 = 30_000.0;
/// Highest believable house load, W. Bounded by the same supply and inverter
/// as the grid, so the same limit.
const LOAD_MAX_W: f64 = 30_000.0;
/// Battery state of charge is a percentage.
const SOC_MAX_PCT: f64 = 100.0;

/// One reading of the plant. A field is `None` when the inverter said the
/// value is unavailable.
///
/// The signs are chosen to read naturally, which means two of them are the
/// opposite of what the inverter sends.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, sqlx::FromRow)]
pub struct Sample {
    /// Unix seconds, UTC, when the read finished.
    pub ts: i64,
    /// Solar power in W.
    pub pv_w: Option<f64>,
    /// House consumption in W.
    pub load_w: Option<f64>,
    /// Grid power in W: positive = importing, negative = exporting.
    /// (The inverter's register says positive = exporting, so we negate it.)
    pub grid_w: Option<f64>,
    /// Battery power in W: positive = charging, negative = discharging.
    /// (The inverter's register says negative = charging, so we negate it.)
    pub battery_w: Option<f64>,
    /// Battery state of charge, 0 to 100.
    pub soc_pct: Option<f64>,
}

/// The inverter's lifetime grid energy counters. They count what the battery's
/// fast hunting around 0 W adds up to, which the 15-second samples never see.
/// A field is `None` when the inverter said the value is unavailable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    /// Unix seconds, UTC, when the read finished.
    pub ts: i64,
    /// Total energy imported from the grid, Wh (100 Wh steps).
    pub import_wh: Option<f64>,
    /// Total energy exported to the grid, Wh (100 Wh steps).
    pub export_wh: Option<f64>,
}

/// The newest sample, shared between the poller and the web server so
/// `/api/live` can answer without a database read. Cloning gives another
/// handle to the same slot.
#[derive(Clone, Default)]
pub struct Latest(Arc<Mutex<Option<Sample>>>);

impl Latest {
    pub fn set(&self, sample: Sample) {
        // A poisoned lock only means a panic elsewhere; the value is still fine.
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(sample);
    }

    pub fn get(&self) -> Option<Sample> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The inverter's control settings, as last read. A field is `None` when the
/// inverter said the value is unavailable or refused to answer for it. The
/// `ems_mode`, `forced_command` and `running_state` fields are the raw register
/// values; see the `*_label` functions for their names.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct Settings {
    /// Unix seconds, UTC, when the read finished.
    pub ts: i64,
    pub ems_mode: Option<u16>,
    pub forced_command: Option<u16>,
    /// Power used when the mode is forced, W.
    pub forced_power_w: Option<u32>,
    /// Battery charge ceiling, %.
    pub max_soc_pct: Option<f64>,
    /// Battery discharge floor, %.
    pub min_soc_pct: Option<f64>,
    pub export_limit_w: Option<u32>,
    /// `None` if the mode register holds neither "on" nor "off".
    pub export_limit_enabled: Option<bool>,
    /// SOC kept back for backup power, %.
    pub backup_reserve_pct: Option<u16>,
    pub max_charge_w: Option<u32>,
    pub max_discharge_w: Option<u32>,
    pub running_state: Option<u16>,
}

/// The newest settings, shared between the poller and the web server like
/// [`Latest`].
#[derive(Clone, Default)]
pub struct LatestSettings(Arc<Mutex<Option<Settings>>>);

impl LatestSettings {
    pub fn set(&self, settings: Settings) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings);
    }

    pub fn get(&self) -> Option<Settings> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Joins two registers into one number. The low word comes first.
fn words_to_u32(words: &[u16]) -> u32 {
    // `u32::from` widens without any risk of losing bits, unlike `as`.
    u32::from(words[0]) | (u32::from(words[1]) << 16)
}

/// Decodes two registers as an unsigned 32-bit value.
/// `0x7FFFFFFF` (unavailable) gives `None`.
fn decode_u32(words: &[u16]) -> Option<u32> {
    Some(words_to_u32(words)).filter(|&v| v != UNAVAILABLE)
}

/// Decodes two registers as a signed 32-bit value.
/// `0x7FFFFFFF` (unavailable) gives `None`.
fn decode_i32(words: &[u16]) -> Option<i32> {
    let raw = words_to_u32(words);
    // Same bits, read as signed. Casting `u32 as i32` does exactly that.
    (raw != UNAVAILABLE).then_some(raw as i32)
}

/// Keeps `watts` only if its size is within `max`. Otherwise logs at debug
/// level (the poll runs every 15 s, so not louder) and gives `None`.
fn within(name: &str, watts: f64, max: f64) -> Option<f64> {
    if watts.abs() > max {
        debug!(value = watts, max, "discarding out-of-range {name} reading");
        None
    } else {
        Some(watts)
    }
}

/// Solar power in W from registers 5016..=5017. `0x7FFFFFFF` and
/// `0xFFFFFFFF` (unavailable), or more than [`PV_MAX_W`], give `None`.
pub fn decode_pv(words: &[u16]) -> Option<f64> {
    let raw = decode_u32(words).filter(|&v| v != UNAVAILABLE_U32_ALL_ONES)?;
    within("PV", f64::from(raw), PV_MAX_W)
}

/// Battery power in W from registers 5213..=5214, with our sign
/// (positive = charging).
pub fn decode_battery(words: &[u16]) -> Option<f64> {
    // Negate as an i64 so that 0 stays 0 (negating the float 0.0 gives -0.0,
    // which prints as "-0.00").
    let watts = decode_i32(words).map(|v| -i64::from(v) as f64)?;
    within("battery", watts, BATTERY_MAX_W)
}

/// State of charge in % from register 13022 (0.1 % per unit). `0xFFFF`
/// (unavailable) or a value outside 0 to 100 gives `None`.
fn decode_soc(raw: u16) -> Option<f64> {
    if raw == UNAVAILABLE_U16 {
        return None;
    }
    let pct = f64::from(raw) / 10.0;
    if pct > SOC_MAX_PCT {
        debug!(value = pct, "discarding out-of-range SOC reading");
        return None;
    }
    Some(pct)
}

/// Turns the 16-register block starting at 13007 into
/// `(load_w, grid_w, soc_pct)`.
pub fn decode_block(words: &[u16]) -> (Option<f64>, Option<f64>, Option<f64>) {
    // Offsets are the register number minus 13007.
    let load = decode_i32(&words[0..2])
        .map(f64::from)
        .and_then(|w| within("load", w, LOAD_MAX_W));
    // Register: positive = exporting. We want positive = importing.
    let grid = decode_i32(&words[2..4])
        .map(|v| -i64::from(v) as f64)
        .and_then(|w| within("grid", w, GRID_MAX_W));
    (load, grid, decode_soc(words[15]))
}

/// Builds a [`Sample`] from the three register reads.
pub fn decode_sample(ts: i64, pv: &[u16], battery: &[u16], block: &[u16]) -> Sample {
    let (load_w, grid_w, soc_pct) = decode_block(block);
    Sample {
        ts,
        pv_w: decode_pv(pv),
        load_w,
        grid_w,
        battery_w: decode_battery(battery),
        soc_pct,
    }
}

/// Turns a lifetime energy counter (two registers, 0.1 kWh) into Wh. The
/// unavailable values and 0 give `None`: a lifetime total of 0 is not real.
fn decode_energy(words: &[u16]) -> Option<f64> {
    let raw = decode_u32(words).filter(|&v| v != UNAVAILABLE_U32_ALL_ONES && v != 0)?;
    Some(f64::from(raw) * METER_UNIT_WH)
}

/// Builds a [`Meter`] from the 11-register block starting at 13036.
pub fn decode_meter(ts: i64, words: &[u16]) -> Meter {
    // Offsets are the register number minus 13036.
    Meter {
        ts,
        import_wh: decode_energy(&words[0..2]),
        export_wh: decode_energy(&words[9..11]),
    }
}

/// Name of the EMS mode in register 13049.
pub fn ems_mode_label(raw: u16) -> String {
    match raw {
        0 => "Self-consumption".to_string(),
        2 => "Forced (compulsory) mode".to_string(),
        3 => "External EMS".to_string(),
        4 => "VPP".to_string(),
        8 => "Microgrid".to_string(),
        other => format!("Unknown ({other})"),
    }
}

/// Name of the forced charge/discharge command in register 13050.
pub fn forced_command_label(raw: u16) -> String {
    match raw {
        0xAA => "Forced charge".to_string(),
        0xBB => "Forced discharge".to_string(),
        0xCC => "Stop".to_string(),
        other => format!("Unknown (0x{other:02X})"),
    }
}

/// Name of the running state in register 12999.
pub fn running_state_label(raw: u16) -> String {
    let name = match raw {
        0x0000 | 0x0040 => "Running",
        0x0001 | 0x8000 => "Stop",
        0x0002 | 0x1300 => "Key stop",
        0x0004 | 0x1500 => "Emergency stop",
        0x0008 | 0x1400 => "Standby",
        0x0010 | 0x1200 => "Initial standby",
        0x0020 | 0x1600 => "Starting",
        0x0041 => "Off-grid charge",
        0x0080 | 0x8100 => "Derating",
        0x0100 | 0x5500 => "Fault",
        0x0200 => "Update failed",
        0x0400 => "Maintain mode",
        0x0800 => "Forced mode",
        0x1000 => "Off-grid",
        0x1111 => "Uninitialized",
        0x2500 => "Communication fault",
        0x2501 => "Restarting",
        0x4000 => "External EMS mode",
        0x4001 => "Emergency charging",
        0x8200 => "Dispatch running",
        0x9100 => "Warning running",
        other => return format!("Unknown (0x{other:04X})"),
    };
    name.to_string()
}

/// Keeps a register value unless it is the "unavailable" all-ones.
fn available(raw: u16) -> Option<u16> {
    Some(raw).filter(|&v| v != UNAVAILABLE_U16)
}

/// Splits registers 13049..=13051 into `(ems_mode, forced_command,
/// forced_power_w)`.
pub fn decode_ems(words: &[u16]) -> (Option<u16>, Option<u16>, Option<u32>) {
    (
        available(words[0]),
        available(words[1]),
        available(words[2]).map(u32::from),
    )
}

/// Splits registers 13057..=13058 into `(max_soc_pct, min_soc_pct)`.
pub fn decode_soc_limits(words: &[u16]) -> (Option<f64>, Option<f64>) {
    (decode_soc(words[0]), decode_soc(words[1]))
}

/// Export limit mode: on, off, or `None` for anything else.
pub fn decode_export_enabled(raw: u16) -> Option<bool> {
    match raw {
        EXPORT_LIMIT_ON => Some(true),
        EXPORT_LIMIT_OFF => Some(false),
        _ => None,
    }
}

/// Backup reserve in % from register 13099. A value over 100 gives `None`.
pub fn decode_backup_reserve(raw: u16) -> Option<u16> {
    available(raw).filter(|&v| v <= 100)
}

/// Splits registers 33046..=33047 into `(max_charge_w, max_discharge_w)`.
pub fn decode_battery_limits(words: &[u16]) -> (Option<u32>, Option<u32>) {
    let watts = |raw: u16| available(raw).map(|v| u32::from(v) * BATTERY_LIMIT_UNIT_W);
    (watts(words[0]), watts(words[1]))
}

/// What one settings read got from the inverter. A group is `None` when the
/// inverter refused it, so only its own fields end up missing.
#[derive(Debug, Default)]
struct RawSettings {
    ems: Option<Vec<u16>>,
    soc_limits: Option<Vec<u16>>,
    export_limit: Option<Vec<u16>>,
    export_mode: Option<Vec<u16>>,
    backup_reserve: Option<Vec<u16>>,
    battery_limits: Option<Vec<u16>>,
    running_state: Option<Vec<u16>>,
}

impl RawSettings {
    fn decode(&self, ts: i64) -> Settings {
        let (ems_mode, forced_command, forced_power_w) = match &self.ems {
            Some(w) => decode_ems(w),
            None => (None, None, None),
        };
        let (max_soc_pct, min_soc_pct) = match &self.soc_limits {
            Some(w) => decode_soc_limits(w),
            None => (None, None),
        };
        let (max_charge_w, max_discharge_w) = match &self.battery_limits {
            Some(w) => decode_battery_limits(w),
            None => (None, None),
        };
        // A group of one register: its first word.
        let first = |group: &Option<Vec<u16>>| group.as_ref().map(|w| w[0]);
        Settings {
            ts,
            ems_mode,
            forced_command,
            forced_power_w,
            max_soc_pct,
            min_soc_pct,
            export_limit_w: first(&self.export_limit).and_then(available).map(u32::from),
            export_limit_enabled: first(&self.export_mode).and_then(decode_export_enabled),
            backup_reserve_pct: first(&self.backup_reserve).and_then(decode_backup_reserve),
            max_charge_w,
            max_discharge_w,
            running_state: first(&self.running_state).and_then(available),
        }
    }
}

/// A log line if the EMS mode or forced command changed between two reads,
/// e.g. `inverter mode Self-consumption -> Forced (compulsory) mode`. A value
/// that is missing in either read is not a change.
pub fn describe_change(old: &Settings, new: &Settings) -> Option<String> {
    let mut parts = Vec::new();
    if let (Some(a), Some(b)) = (old.ems_mode, new.ems_mode)
        && a != b
    {
        parts.push(format!(
            "inverter mode {} -> {}",
            ems_mode_label(a),
            ems_mode_label(b)
        ));
    }
    if let (Some(a), Some(b)) = (old.forced_command, new.forced_command)
        && a != b
    {
        parts.push(format!(
            "forced command {} -> {}",
            forced_command_label(a),
            forced_command_label(b)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Whether the running state is plain "Running" or "Dispatch running" (taking
/// orders from outside). The inverter flips between these often, so a change
/// from one to the other is not worth saving.
fn running_alike(raw: u16) -> bool {
    matches!(raw, 0x0000 | 0x0040 | 0x8200)
}

/// What to save after a settings read: `new` with any missing value filled in
/// from the last saved row (a register the inverter refused is not a change),
/// or `None` when nothing but the time differs from `saved`.
pub fn settings_to_save(saved: Option<&Settings>, new: &Settings) -> Option<Settings> {
    let Some(old) = saved else {
        return Some(new.clone());
    };
    let merged = Settings {
        ts: new.ts,
        ems_mode: new.ems_mode.or(old.ems_mode),
        forced_command: new.forced_command.or(old.forced_command),
        forced_power_w: new.forced_power_w.or(old.forced_power_w),
        max_soc_pct: new.max_soc_pct.or(old.max_soc_pct),
        min_soc_pct: new.min_soc_pct.or(old.min_soc_pct),
        export_limit_w: new.export_limit_w.or(old.export_limit_w),
        export_limit_enabled: new.export_limit_enabled.or(old.export_limit_enabled),
        backup_reserve_pct: new.backup_reserve_pct.or(old.backup_reserve_pct),
        max_charge_w: new.max_charge_w.or(old.max_charge_w),
        max_discharge_w: new.max_discharge_w.or(old.max_discharge_w),
        running_state: new.running_state.or(old.running_state),
    };
    // Compare with the time made equal, and with a flip between "Running" and
    // "Dispatch running" ignored: the inverter goes back and forth between the
    // two every minute or so while the retailer's service is connected.
    let running_state = match (old.running_state, merged.running_state) {
        (Some(a), Some(b)) if running_alike(a) && running_alike(b) => old.running_state,
        _ => merged.running_state,
    };
    let same = Settings {
        ts: old.ts,
        running_state,
        ..merged.clone()
    } == *old;
    (!same).then_some(merged)
}

/// Two short lines for people, e.g.
/// `Inverter: Running, mode Self-consumption, forced command Stop at 0 W`.
pub fn describe_settings(s: &Settings) -> String {
    let text = |v: Option<String>| v.unwrap_or_else(|| "n/a".to_string());
    let kw = |w: Option<u32>| text(w.map(|w| format!("{:.1} kW", f64::from(w) / 1000.0)));
    let pct = |p: Option<f64>| text(p.map(|p| format!("{p:.1}%")));
    let export = match (s.export_limit_enabled, s.export_limit_w) {
        (Some(true), w) => format!("limit {}", kw(w)),
        (Some(false), _) => "no limit".to_string(),
        (None, _) => "n/a".to_string(),
    };
    format!(
        "Inverter: {}, mode {}, forced command {} at {}\n\
         Battery: SOC {} to {}, backup reserve {}, charge/discharge limit {} / {}, export {export}",
        text(s.running_state.map(running_state_label)),
        text(s.ems_mode.map(ems_mode_label)),
        text(s.forced_command.map(forced_command_label)),
        kw(s.forced_power_w),
        pct(s.min_soc_pct),
        pct(s.max_soc_pct),
        text(s.backup_reserve_pct.map(|p| format!("{p}%"))),
        kw(s.max_charge_w),
        kw(s.max_discharge_w),
    )
}

/// One line for people, e.g.
/// `PV 4.90 kW, load 0.71 kW, grid 0.00 kW (import), battery 4.23 kW (charging), SOC 43.4%`.
pub fn describe(sample: &Sample) -> String {
    let kw = |watts: Option<f64>| match watts {
        // Round to whole 10 W first. Dividing 4895 by 1000 gives 4.8949999...,
        // which `{:.2}` would show as 4.89 instead of 4.90.
        Some(w) => format!("{:.2} kW", (w.abs() / 10.0).round() / 100.0),
        None => "n/a".to_string(),
    };
    let grid_dir = match sample.grid_w {
        Some(w) if w < 0.0 => " (export)",
        Some(_) => " (import)",
        None => "",
    };
    let battery_dir = match sample.battery_w {
        Some(w) if w > 0.0 => " (charging)",
        Some(w) if w < 0.0 => " (discharging)",
        Some(_) => " (idle)",
        None => "",
    };
    let soc = match sample.soc_pct {
        Some(s) => format!("{s:.1}%"),
        None => "n/a".to_string(),
    };
    format!(
        "PV {}, load {}, grid {}{grid_dir}, battery {}{battery_dir}, SOC {soc}",
        kw(sample.pv_w),
        kw(sample.load_w),
        kw(sample.grid_w),
        kw(sample.battery_w),
    )
}

/// One line for people, e.g. `grid totals: import 12345.6 kWh, export 23456.7 kWh`.
pub fn describe_meter(meter: &Meter) -> String {
    let kwh = |wh: Option<f64>| match wh {
        Some(w) => format!("{:.1} kWh", w / 1000.0),
        None => "n/a".to_string(),
    };
    format!(
        "grid totals: import {}, export {}",
        kwh(meter.import_wh),
        kwh(meter.export_wh)
    )
}

/// Counts successful and failed reads, for the `modbus-test --watch` summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tally {
    pub ok: u32,
    pub failed: u32,
}

impl fmt::Display for Tally {
    /// `"N ok, M failed (x%)"`, where x is the share of failed reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.ok + self.failed;
        let percent = if total == 0 {
            0.0
        } else {
            f64::from(self.failed) * 100.0 / f64::from(total)
        };
        write!(f, "{} ok, {} failed ({percent:.1}%)", self.ok, self.failed)
    }
}

/// Reads samples from the inverter. It connects on the first read and
/// reconnects after any error.
pub struct Reader {
    config: Modbus,
    /// `None` until connected, and again after an error.
    connection: Option<ModbusContext>,
}

impl Reader {
    pub fn new(config: Modbus) -> Self {
        Self {
            config,
            connection: None,
        }
    }

    /// Does the three reads and returns a [`Sample`], plus the grid energy
    /// counters from a fourth read. The inverter refusing that fourth read
    /// only gives `None` for the meter. On any other error the connection is
    /// dropped, so calling this again reconnects.
    pub async fn read_sample(&mut self) -> Result<(Sample, Option<Meter>)> {
        let result = self.try_read().await;
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    async fn try_read(&mut self) -> Result<(Sample, Option<Meter>)> {
        let ctx = match &mut self.connection {
            Some(ctx) => ctx,
            // `insert` stores the new connection and gives back a reference to it.
            None => self.connection.insert(connect(&self.config).await?),
        };

        let pv = read_registers(ctx, PV_POWER_REG, 2).await?;
        sleep(REQUEST_PAUSE).await;
        let battery = read_registers(ctx, BATTERY_POWER_REG, 2).await?;
        sleep(REQUEST_PAUSE).await;
        let block = read_registers(ctx, BLOCK_START, BLOCK_LEN).await?;

        sleep(REQUEST_PAUSE).await;
        let meter = refusable(read_input(ctx, METER_START, METER_LEN).await?, METER_START);

        let ts = db::now();
        Ok((
            decode_sample(ts, &pv, &battery, &block),
            meter.map(|words| decode_meter(ts, &words)),
        ))
    }

    /// Reads the control settings, one small request per group. An answer
    /// with an error code from the inverter only blanks that group's fields;
    /// a transport error or timeout fails the read and drops the connection,
    /// like [`Reader::read_sample`].
    pub async fn read_settings(&mut self) -> Result<Settings> {
        let result = self.try_read_settings().await;
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    async fn try_read_settings(&mut self) -> Result<Settings> {
        let ctx = match &mut self.connection {
            Some(ctx) => ctx,
            None => self.connection.insert(connect(&self.config).await?),
        };

        let ems = refusable(read_holding(ctx, EMS_START, EMS_LEN).await?, EMS_START);
        sleep(REQUEST_PAUSE).await;
        let soc_limits = refusable(
            read_holding(ctx, SOC_LIMITS_START, SOC_LIMITS_LEN).await?,
            SOC_LIMITS_START,
        );
        sleep(REQUEST_PAUSE).await;
        let export_limit = refusable(
            read_holding(ctx, EXPORT_LIMIT_REG, 1).await?,
            EXPORT_LIMIT_REG,
        );
        sleep(REQUEST_PAUSE).await;
        let export_mode = refusable(
            read_holding(ctx, EXPORT_MODE_REG, 1).await?,
            EXPORT_MODE_REG,
        );
        sleep(REQUEST_PAUSE).await;
        let backup_reserve = refusable(
            read_holding(ctx, BACKUP_RESERVE_REG, 1).await?,
            BACKUP_RESERVE_REG,
        );
        sleep(REQUEST_PAUSE).await;
        let battery_limits = refusable(
            read_holding(ctx, BATTERY_LIMITS_START, BATTERY_LIMITS_LEN).await?,
            BATTERY_LIMITS_START,
        );
        sleep(REQUEST_PAUSE).await;
        let running_state = refusable(
            read_input(ctx, RUNNING_STATE_REG, 1).await?,
            RUNNING_STATE_REG,
        );

        let raw = RawSettings {
            ems,
            soc_limits,
            export_limit,
            export_mode,
            backup_reserve,
            battery_limits,
            running_state,
        };
        Ok(raw.decode(db::now()))
    }
}

/// While Modbus keeps failing, the first failure is logged and then one in
/// every this many, so a switched-off dongle does not fill the log.
const LOG_EVERY_N_FAILURES: u32 = 20;

/// Decides which poll results are worth a log line. It is a plain struct
/// with no logging of its own, so it can be unit tested.
#[derive(Debug, Default)]
struct FailureLog {
    /// Failures in a row since the last success.
    in_a_row: u32,
}

impl FailureLog {
    /// Call after a failed read. Returns `true` if this one should be logged:
    /// the 1st, 21st, 41st... failure in a row.
    fn failed(&mut self) -> bool {
        self.in_a_row += 1;
        (self.in_a_row - 1).is_multiple_of(LOG_EVERY_N_FAILURES)
    }

    /// Call after a good read. Returns how many failures came before it if
    /// we are recovering from some, so "Modbus back" can be logged.
    fn succeeded(&mut self) -> Option<u32> {
        // `take` swaps the value for 0 and gives back the old one.
        let failures = std::mem::take(&mut self.in_a_row);
        (failures > 0).then_some(failures)
    }
}

/// The control settings change rarely, so they are read on every this many
/// ticks (the first tick included): about once a minute at the default interval.
const SETTINGS_EVERY_N_TICKS: u32 = 4;

/// The `(import_wh, export_wh)` to save for `meter`, or `None` to skip it:
/// when a value is unavailable, nothing changed since `saved`, or a total
/// went down (a lifetime counter only grows, so that is a glitch).
fn meter_to_save(saved: Option<(f64, f64)>, meter: &Meter) -> Option<(f64, f64)> {
    let totals = (meter.import_wh?, meter.export_wh?);
    match saved {
        Some(saved) if saved == totals => None,
        Some(saved) if totals.0 < saved.0 || totals.1 < saved.1 => {
            debug!(?saved, ?totals, "discarding grid totals that went down");
            None
        }
        _ => Some(totals),
    }
}

/// Reads a sample every `interval_secs`, keeps it in `latest` and saves it,
/// until `shutdown` completes. Every few ticks it also reads the control
/// settings into `latest_settings`. Failures are logged (throttled) and never
/// end the loop: the dongle is flaky, and the rest of `serve` must keep going
/// without it.
pub async fn run_poll(
    config: Modbus,
    pool: &SqlitePool,
    latest: Latest,
    latest_settings: LatestSettings,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs));
    // After a slow read, don't make up for it with a burst of reads.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reader = Reader::new(config);
    let mut failures = FailureLog::default();
    let mut settings_failures = FailureLog::default();
    let mut ticks: u32 = 0;
    let mut previous_settings: Option<Settings> = None;
    // The newest saved row, so a restart does not save the same settings again.
    // The grid totals last saved, so only a change is saved again.
    let mut saved_meter = match db::latest_grid_meter(pool).await {
        Ok(saved) => saved.map(|(_, import_wh, export_wh)| (import_wh, export_wh)),
        Err(err) => {
            warn!("could not read saved grid totals: {err:#}");
            None
        }
    };
    let mut saved_settings = match db::latest_inverter_settings(pool).await {
        Ok(saved) => saved,
        Err(err) => {
            warn!("could not read saved inverter settings: {err:#}");
            None
        }
    };

    tokio::pin!(shutdown);
    info!("Modbus polling started");
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = ticker.tick() => {}
        }
        // Dropping an unfinished read on Ctrl+C is fine; nothing is half-saved.
        let result = tokio::select! {
            _ = &mut shutdown => break,
            result = reader.read_sample() => result,
        };
        match result {
            Ok((sample, meter)) => {
                if let Some(count) = failures.succeeded() {
                    info!(failed_reads = count, "Modbus back");
                }
                latest.set(sample);
                if let Err(err) = db::insert_live_sample(pool, &sample).await {
                    warn!("could not save Modbus sample: {err:#}");
                }
                if let Some(meter) = meter
                    && let Some(totals) = meter_to_save(saved_meter, &meter)
                {
                    match db::insert_grid_meter(pool, meter.ts, totals.0, totals.1).await {
                        Ok(()) => saved_meter = Some(totals),
                        Err(err) => warn!("could not save grid totals: {err:#}"),
                    }
                }
            }
            Err(err) => {
                if failures.failed() {
                    warn!("Modbus read failed: {err:#}");
                }
            }
        }

        let read_settings_now = ticks.is_multiple_of(SETTINGS_EVERY_N_TICKS);
        ticks = ticks.wrapping_add(1);
        if !read_settings_now {
            continue;
        }
        let result = tokio::select! {
            _ = &mut shutdown => break,
            result = reader.read_settings() => result,
        };
        match result {
            Ok(settings) => {
                if let Some(count) = settings_failures.succeeded() {
                    info!(failed_reads = count, "Modbus settings back");
                }
                if let Some(old) = &previous_settings
                    && let Some(change) = describe_change(old, &settings)
                {
                    info!("{change}");
                }
                previous_settings = Some(settings.clone());
                if let Some(row) = settings_to_save(saved_settings.as_ref(), &settings) {
                    match db::insert_inverter_settings(pool, &row).await {
                        Ok(()) => saved_settings = Some(row),
                        Err(err) => warn!("could not save inverter settings: {err:#}"),
                    }
                }
                latest_settings.set(settings);
            }
            Err(err) => {
                if settings_failures.failed() {
                    warn!("Modbus settings read failed: {err:#}");
                }
            }
        }
    }
    info!("Modbus polling stopped");
    Ok(())
}

/// Opens a TCP connection to the inverter, with a timeout.
async fn connect(config: &Modbus) -> Result<ModbusContext> {
    let target = format!("{}:{}", config.host, config.port);
    let connecting = async {
        // `lookup_host` also accepts host names, not just IP addresses.
        let addr = lookup_host(&target)
            .await?
            .next()
            .ok_or_else(|| anyhow!("no address found"))?;
        let ctx = tcp::connect_slave(addr, Slave(config.unit)).await?;
        anyhow::Ok(ctx)
    };
    match timeout(REQUEST_TIMEOUT, connecting).await {
        Ok(result) => result.with_context(|| format!("could not connect to {target}")),
        Err(_) => bail!("connecting to {target} timed out"),
    }
}

/// What a register read gives: the words, or the error code the inverter
/// answered with instead.
type Answer = Result<Vec<u16>, ExceptionCode>;

/// Checks a reply to a read of `count` registers at `address`. The error code
/// from the device is passed on inside the `Ok`; transport errors, timeouts
/// and a wrong number of words are errors.
fn check_reply(
    reply: Result<Result<Answer, tokio_modbus::Error>, tokio::time::error::Elapsed>,
    address: u16,
    count: u16,
) -> Result<Answer> {
    // tokio-modbus has two layers of result: the outer one is for transport
    // errors, the inner one for an error answer from the device.
    let reply = match reply {
        Ok(reply) => reply,
        Err(_) => bail!("reading registers at {address} timed out"),
    };
    let answer = reply.with_context(|| format!("reading registers at {address} failed"))?;
    if let Ok(words) = &answer
        && words.len() != usize::from(count)
    {
        bail!(
            "asked for {count} registers at {address}, got {}",
            words.len()
        );
    }
    Ok(answer)
}

/// Reads `count` input registers (function 04) starting at `address`.
async fn read_input(ctx: &mut ModbusContext, address: u16, count: u16) -> Result<Answer> {
    let reply = timeout(REQUEST_TIMEOUT, ctx.read_input_registers(address, count)).await;
    check_reply(reply, address, count)
}

/// Reads `count` holding registers (function 03) starting at `address`.
async fn read_holding(ctx: &mut ModbusContext, address: u16, count: u16) -> Result<Answer> {
    let reply = timeout(REQUEST_TIMEOUT, ctx.read_holding_registers(address, count)).await;
    check_reply(reply, address, count)
}

/// Like [`read_input`], but an error code from the inverter is an error too.
async fn read_registers(ctx: &mut ModbusContext, address: u16, count: u16) -> Result<Vec<u16>> {
    read_input(ctx, address, count)
        .await?
        .map_err(|code| anyhow!("inverter refused registers at {address}: {code}"))
}

/// For the settings: a refused group gives `None` (and a debug line, as the
/// dongle refuses some registers on some firmware) instead of failing the read.
fn refusable(answer: Answer, address: u16) -> Option<Vec<u16>> {
    match answer {
        Ok(words) => Some(words),
        Err(code) => {
            debug!("inverter refused registers at {address}: {code}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Ready, ready};
    use std::net::SocketAddr;

    use tokio::net::TcpListener;
    use tokio_modbus::server::Service;
    use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
    use tokio_modbus::{ExceptionCode, Request, Response, SlaveRequest};

    use super::*;

    // Real readings from a Sungrow inverter.
    const BLOCK: [u16; 16] = [
        708, 0, 0, 0, 55, 13409, 0, 0, 0, 36, 40835, 1, 2659, 65374, 4248, 434,
    ];
    const PV: [u16; 2] = [0x131F, 0x0000];
    const BATTERY: [u16; 2] = [0xEF7C, 0xFFFF];

    #[test]
    fn decodes_real_readings() {
        assert_eq!(decode_pv(&PV), Some(4895.0));
        assert_eq!(decode_battery(&BATTERY), Some(4228.0));
        let (load, grid, soc) = decode_block(&BLOCK);
        assert_eq!(load, Some(708.0));
        assert_eq!(grid, Some(0.0));
        assert!(grid.unwrap().is_sign_positive(), "zero must not be -0.0");
        assert!((soc.unwrap() - 43.4).abs() < 1e-9);
    }

    #[test]
    fn soc_unavailable_or_out_of_range_is_none() {
        let mut block = BLOCK;
        block[15] = 0xFFFF;
        assert_eq!(decode_block(&block).2, None);
        block[15] = 1000;
        assert_eq!(decode_block(&block).2, Some(100.0));
        block[15] = 1001;
        assert_eq!(decode_block(&block).2, None);
        block[15] = 0;
        assert_eq!(decode_block(&block).2, Some(0.0));
    }

    #[test]
    fn pv_all_ones_or_too_high_is_none() {
        assert_eq!(decode_pv(&[0xFFFF, 0xFFFF]), None);
        assert_eq!(decode_pv(&[20_000, 0]), Some(20_000.0));
        assert_eq!(decode_pv(&[20_001, 0]), None);
    }

    #[test]
    fn out_of_range_power_is_none() {
        let mut block = BLOCK;
        // Register says exporting 40 kW: more than a 63 A supply can carry.
        block[2] = 40_000;
        block[3] = 0;
        assert_eq!(decode_block(&block).1, None);
        // Same size, importing (negative register value).
        let raw = (-40_000i32) as u32;
        block[2] = (raw & 0xFFFF) as u16;
        block[3] = (raw >> 16) as u16;
        assert_eq!(decode_block(&block).1, None);
        // Load and battery too.
        block[0] = 40_000;
        assert_eq!(decode_block(&block).0, None);
        assert_eq!(decode_battery(&[40_000, 0]), None);
        assert_eq!(decode_battery(&[30_000, 0]), Some(-30_000.0));
    }

    #[test]
    fn words_are_low_first() {
        assert_eq!(words_to_u32(&[0x0001, 0x0002]), 0x0002_0001);
    }

    #[test]
    fn signs_are_flipped_for_grid_and_battery() {
        // Register says exporting 1000 W: we say grid is -1000 W.
        let mut block = BLOCK;
        block[2] = 1000;
        assert_eq!(decode_block(&block).1, Some(-1000.0));
        // Register says importing 250 W (-250): we say +250 W.
        block[2] = (-250i32 as u32 & 0xFFFF) as u16;
        block[3] = 0xFFFF;
        assert_eq!(decode_block(&block).1, Some(250.0));
        // Register says discharging 500 W: we say battery is -500 W.
        assert_eq!(decode_battery(&[500, 0]), Some(-500.0));
    }

    #[test]
    fn unavailable_becomes_none() {
        let na = [0xFFFF, 0x7FFF];
        assert_eq!(decode_pv(&na), None);
        assert_eq!(decode_battery(&na), None);
        let mut block = BLOCK;
        block[0] = 0xFFFF;
        block[1] = 0x7FFF;
        block[2] = 0xFFFF;
        block[3] = 0x7FFF;
        let (load, grid, soc) = decode_block(&block);
        assert_eq!((load, grid), (None, None));
        assert!(soc.is_some());
    }

    /// Import 12345.0 kWh (123450 = 0x0001_E23A) and export 23456.0 kWh
    /// (234560 = 0x0003_9440), low word first, with filler between.
    const METER: [u16; 11] = [0xE23A, 0x0001, 0, 0, 0, 0, 0, 0, 0, 0x9440, 0x0003];

    #[test]
    fn decodes_the_grid_meter() {
        let meter = decode_meter(7, &METER);
        assert_eq!(meter.ts, 7);
        assert_eq!(meter.import_wh, Some(12_345_000.0));
        assert_eq!(meter.export_wh, Some(23_456_000.0));
    }

    #[test]
    fn grid_meter_unavailable_values_are_none() {
        let mut words = METER;
        // Unavailable as 0x7FFFFFFF.
        words[0] = 0xFFFF;
        words[1] = 0x7FFF;
        // Unavailable as all ones.
        words[9] = 0xFFFF;
        words[10] = 0xFFFF;
        let meter = decode_meter(0, &words);
        assert_eq!((meter.import_wh, meter.export_wh), (None, None));
        // A lifetime total of 0 is not real either.
        words[0] = 0;
        words[1] = 0;
        words[9] = 0;
        words[10] = 0;
        let meter = decode_meter(0, &words);
        assert_eq!((meter.import_wh, meter.export_wh), (None, None));
    }

    #[test]
    fn describes_the_grid_meter() {
        assert_eq!(
            describe_meter(&decode_meter(0, &METER)),
            "grid totals: import 12345.0 kWh, export 23456.0 kWh"
        );
        let meter = Meter {
            import_wh: None,
            ..decode_meter(0, &METER)
        };
        assert_eq!(
            describe_meter(&meter),
            "grid totals: import n/a, export 23456.0 kWh"
        );
    }

    #[test]
    fn saves_grid_totals_only_when_they_grow() {
        let meter = |import_wh, export_wh| Meter {
            ts: 0,
            import_wh,
            export_wh,
        };
        let saved = Some((100.0, 200.0));
        // The first reading is saved; so is a change in either value.
        assert_eq!(
            meter_to_save(None, &meter(Some(100.0), Some(200.0))),
            Some((100.0, 200.0))
        );
        assert_eq!(
            meter_to_save(saved, &meter(Some(200.0), Some(200.0))),
            Some((200.0, 200.0))
        );
        assert_eq!(
            meter_to_save(saved, &meter(Some(100.0), Some(300.0))),
            Some((100.0, 300.0))
        );
        // No change, a value that went down, or a missing value: skip.
        assert_eq!(meter_to_save(saved, &meter(Some(100.0), Some(200.0))), None);
        assert_eq!(meter_to_save(saved, &meter(Some(0.0), Some(300.0))), None);
        assert_eq!(meter_to_save(saved, &meter(None, Some(300.0))), None);
        assert_eq!(meter_to_save(saved, &meter(Some(300.0), None)), None);
    }

    #[test]
    fn describes_a_sample() {
        let sample = decode_sample(0, &PV, &BATTERY, &BLOCK);
        assert_eq!(
            describe(&sample),
            "PV 4.90 kW, load 0.71 kW, grid 0.00 kW (import), battery 4.23 kW (charging), SOC 43.4%"
        );
        let sample = Sample {
            pv_w: None,
            grid_w: Some(-1500.0),
            battery_w: Some(-2000.0),
            ..sample
        };
        let text = describe(&sample);
        assert!(text.starts_with("PV n/a, "));
        assert!(text.contains("grid 1.50 kW (export)"));
        assert!(text.contains("battery 2.00 kW (discharging)"));
    }

    #[test]
    fn failure_log_throttles_and_reports_recovery() {
        let mut log = FailureLog::default();
        // Nothing to report while all is well.
        assert_eq!(log.succeeded(), None);

        // Logged: failure 1, then 21, 41... Not logged: the ones between.
        let logged: Vec<u32> = (1..=45).filter(|_| log.failed()).collect();
        assert_eq!(logged, vec![1, 21, 41]);

        // Recovery reports how many failed, once.
        assert_eq!(log.succeeded(), Some(45));
        assert_eq!(log.succeeded(), None);

        // A new outage logs its first failure again.
        assert!(log.failed());
        assert!(!log.failed());
    }

    #[test]
    fn tally_shows_failure_percentage() {
        assert_eq!(Tally::default().to_string(), "0 ok, 0 failed (0.0%)");
        let tally = Tally { ok: 3, failed: 1 };
        assert_eq!(tally.to_string(), "3 ok, 1 failed (25.0%)");
    }

    // A fake inverter: a Modbus server that answers with the real readings.

    struct FakeInverter {
        unit: u8,
    }

    impl Service for FakeInverter {
        type Request = SlaveRequest<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = Ready<Result<Response, ExceptionCode>>;

        fn call(&self, req: Self::Request) -> Self::Future {
            let answer = match req.request {
                _ if req.slave != self.unit => Err(ExceptionCode::GatewayTargetDevice),
                Request::ReadInputRegisters(5016, 2) => {
                    Ok(Response::ReadInputRegisters(PV.to_vec()))
                }
                Request::ReadInputRegisters(5213, 2) => {
                    Ok(Response::ReadInputRegisters(BATTERY.to_vec()))
                }
                Request::ReadInputRegisters(13007, 16) => {
                    Ok(Response::ReadInputRegisters(BLOCK.to_vec()))
                }
                Request::ReadInputRegisters(13036, 11) => {
                    Ok(Response::ReadInputRegisters(METER.to_vec()))
                }
                Request::ReadInputRegisters(12999, 1) => {
                    Ok(Response::ReadInputRegisters(vec![0x0000]))
                }
                // Forced charge at 5 kW.
                Request::ReadHoldingRegisters(13049, 3) => {
                    Ok(Response::ReadHoldingRegisters(vec![2, 0xAA, 5000]))
                }
                Request::ReadHoldingRegisters(13057, 2) => {
                    Ok(Response::ReadHoldingRegisters(vec![1000, 50]))
                }
                Request::ReadHoldingRegisters(13073, 1) => {
                    Ok(Response::ReadHoldingRegisters(vec![5000]))
                }
                Request::ReadHoldingRegisters(13086, 1) => {
                    Ok(Response::ReadHoldingRegisters(vec![0xAA]))
                }
                Request::ReadHoldingRegisters(13099, 1) => {
                    Ok(Response::ReadHoldingRegisters(vec![10]))
                }
                // 33046 is refused on purpose, like some dongle firmware does.
                _ => Err(ExceptionCode::IllegalDataAddress),
            };
            ready(answer)
        }
    }

    /// Starts the fake inverter on `listener`. Aborting the returned task
    /// also closes its connections, like a dongle that drops them.
    fn start_fake(listener: TcpListener, unit: u8) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let on_connected = move |stream, addr| async move {
                accept_tcp_connection(stream, addr, move |_| Ok(Some(FakeInverter { unit })))
            };
            let _ = Server::new(listener).serve(&on_connected, |_| {}).await;
        })
    }

    async fn local_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    fn config_for(addr: SocketAddr) -> Modbus {
        Modbus {
            host: addr.ip().to_string(),
            port: addr.port(),
            unit: 1,
            interval_secs: 15,
        }
    }

    #[tokio::test]
    async fn reader_reads_a_sample_from_a_fake_inverter() {
        let (listener, addr) = local_listener().await;
        let server = start_fake(listener, 1);

        let mut reader = Reader::new(config_for(addr));
        let (sample, meter) = reader.read_sample().await.unwrap();
        let meter = meter.unwrap();
        assert_eq!(meter.ts, sample.ts);
        assert_eq!(meter.import_wh, Some(12_345_000.0));
        assert_eq!(meter.export_wh, Some(23_456_000.0));
        assert_eq!(sample.pv_w, Some(4895.0));
        assert_eq!(sample.load_w, Some(708.0));
        assert_eq!(sample.grid_w, Some(0.0));
        assert_eq!(sample.battery_w, Some(4228.0));
        assert!((sample.soc_pct.unwrap() - 43.4).abs() < 1e-9);
        assert!(sample.ts > 1_700_000_000);
        // The same connection is used again.
        reader.read_sample().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn reader_reconnects_after_the_connection_is_lost() {
        let (listener, addr) = local_listener().await;
        let server = start_fake(listener, 1);
        let mut reader = Reader::new(config_for(addr));
        reader.read_sample().await.unwrap();

        // The "dongle" goes away: the read fails and the connection is dropped.
        server.abort();
        let _ = server.await;
        assert!(reader.read_sample().await.is_err());
        assert!(reader.connection.is_none());

        // It comes back on the same port: the reader connects again by itself.
        let listener = TcpListener::bind(addr).await.unwrap();
        let server = start_fake(listener, 1);
        assert!(reader.read_sample().await.is_ok());
        server.abort();
    }

    #[tokio::test]
    async fn reader_reports_a_device_error() {
        let (listener, addr) = local_listener().await;
        // The fake only answers unit 7, but we ask unit 1.
        let server = start_fake(listener, 7);
        let mut reader = Reader::new(config_for(addr));
        let err = reader.read_sample().await.unwrap_err();
        assert!(format!("{err:#}").contains("refused"), "{err:#}");
        server.abort();
    }

    #[tokio::test]
    async fn reader_fails_cleanly_when_nothing_is_listening() {
        let (listener, addr) = local_listener().await;
        drop(listener);
        let mut reader = Reader::new(config_for(addr));
        let err = reader.read_sample().await.unwrap_err();
        assert!(format!("{err:#}").contains("could not connect"), "{err:#}");
    }

    #[tokio::test]
    async fn reader_keeps_going_when_one_settings_group_is_refused() {
        let (listener, addr) = local_listener().await;
        let server = start_fake(listener, 1);
        let mut reader = Reader::new(config_for(addr));

        let settings = reader.read_settings().await.unwrap();
        assert!(settings.ts > 1_700_000_000);
        assert_eq!(settings.ems_mode, Some(2));
        assert_eq!(settings.forced_command, Some(0xAA));
        assert_eq!(settings.forced_power_w, Some(5000));
        assert_eq!(settings.max_soc_pct, Some(100.0));
        assert_eq!(settings.min_soc_pct, Some(5.0));
        assert_eq!(settings.export_limit_w, Some(5000));
        assert_eq!(settings.export_limit_enabled, Some(true));
        assert_eq!(settings.backup_reserve_pct, Some(10));
        assert_eq!(settings.running_state, Some(0));
        // The refused group is missing, nothing else.
        assert_eq!(settings.max_charge_w, None);
        assert_eq!(settings.max_discharge_w, None);

        // The connection was kept and still works.
        assert!(reader.connection.is_some());
        reader.read_sample().await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn settings_read_fails_when_the_connection_is_lost() {
        let (listener, addr) = local_listener().await;
        let server = start_fake(listener, 1);
        let mut reader = Reader::new(config_for(addr));
        reader.read_settings().await.unwrap();
        server.abort();
        let _ = server.await;
        assert!(reader.read_settings().await.is_err());
        assert!(reader.connection.is_none());
    }

    #[test]
    fn labels_name_the_modes() {
        assert_eq!(ems_mode_label(0), "Self-consumption");
        assert_eq!(ems_mode_label(2), "Forced (compulsory) mode");
        assert_eq!(ems_mode_label(3), "External EMS");
        assert_eq!(ems_mode_label(4), "VPP");
        assert_eq!(ems_mode_label(8), "Microgrid");
        assert_eq!(ems_mode_label(5), "Unknown (5)");
        assert_eq!(forced_command_label(0xAA), "Forced charge");
        assert_eq!(forced_command_label(0xBB), "Forced discharge");
        assert_eq!(forced_command_label(0xCC), "Stop");
        assert_eq!(forced_command_label(0x01), "Unknown (0x01)");
        assert_eq!(running_state_label(0x0000), "Running");
        assert_eq!(running_state_label(0x0040), "Running");
        assert_eq!(running_state_label(0x0041), "Off-grid charge");
        assert_eq!(running_state_label(0x1400), "Standby");
        assert_eq!(running_state_label(0x4000), "External EMS mode");
        assert_eq!(running_state_label(0x9100), "Warning running");
        assert_eq!(running_state_label(0x1234), "Unknown (0x1234)");
    }

    #[test]
    fn decodes_setting_groups() {
        assert_eq!(decode_ems(&[0, 0xCC, 0]), (Some(0), Some(0xCC), Some(0)));
        assert_eq!(
            decode_ems(&[0xFFFF, 0xBB, 3000]),
            (None, Some(0xBB), Some(3000))
        );
        assert_eq!(decode_soc_limits(&[1000, 50]), (Some(100.0), Some(5.0)));
        assert_eq!(decode_soc_limits(&[0xFFFF, 100]), (None, Some(10.0)));
        assert_eq!(decode_export_enabled(0xAA), Some(true));
        assert_eq!(decode_export_enabled(0x55), Some(false));
        assert_eq!(decode_export_enabled(0), None);
        assert_eq!(decode_backup_reserve(10), Some(10));
        assert_eq!(decode_backup_reserve(0xFFFF), None);
        assert_eq!(decode_backup_reserve(101), None);
        // Units of 10 W: 800 is 8 kW.
        assert_eq!(decode_battery_limits(&[800, 800]), (Some(8000), Some(8000)));
        assert_eq!(decode_battery_limits(&[0xFFFF, 500]), (None, Some(5000)));
    }

    #[test]
    fn refused_groups_only_blank_their_own_fields() {
        let raw = RawSettings {
            ems: Some(vec![0, 0xCC, 0]),
            soc_limits: None,
            export_limit: Some(vec![0xFFFF]),
            export_mode: Some(vec![0x55]),
            backup_reserve: Some(vec![10]),
            battery_limits: Some(vec![800, 800]),
            running_state: None,
        };
        let s = raw.decode(42);
        assert_eq!(s.ts, 42);
        assert_eq!(s.ems_mode, Some(0));
        assert_eq!(s.max_soc_pct, None);
        assert_eq!(s.min_soc_pct, None);
        assert_eq!(s.export_limit_w, None);
        assert_eq!(s.export_limit_enabled, Some(false));
        assert_eq!(s.backup_reserve_pct, Some(10));
        assert_eq!(s.max_charge_w, Some(8000));
        assert_eq!(s.running_state, None);
        // Everything refused: all fields are missing.
        let none = RawSettings::default().decode(1);
        assert_eq!(none.ems_mode, None);
        assert_eq!(none.max_discharge_w, None);
    }

    #[test]
    fn describes_a_mode_change() {
        let before = RawSettings {
            ems: Some(vec![0, 0xCC, 0]),
            ..RawSettings::default()
        }
        .decode(1);
        let after = RawSettings {
            ems: Some(vec![2, 0xAA, 5000]),
            ..RawSettings::default()
        }
        .decode(2);
        assert_eq!(
            describe_change(&before, &after).unwrap(),
            "inverter mode Self-consumption -> Forced (compulsory) mode, \
             forced command Stop -> Forced charge"
        );
        assert_eq!(describe_change(&after, &after), None);
        // A refused group is not a change.
        let blank = RawSettings::default().decode(3);
        assert_eq!(describe_change(&after, &blank), None);
    }

    #[test]
    fn describes_settings_for_people() {
        let s = RawSettings {
            ems: Some(vec![0, 0xCC, 0]),
            soc_limits: Some(vec![1000, 50]),
            export_limit: Some(vec![5000]),
            export_mode: Some(vec![0xAA]),
            backup_reserve: Some(vec![10]),
            battery_limits: None,
            running_state: Some(vec![0]),
        }
        .decode(0);
        let text = describe_settings(&s);
        assert!(text.starts_with(
            "Inverter: Running, mode Self-consumption, forced command Stop at 0.0 kW\n"
        ));
        assert!(text.contains("SOC 5.0% to 100.0%, backup reserve 10%"));
        assert!(text.contains("limit n/a / n/a, export limit 5.0 kW"));
    }

    #[test]
    fn saves_settings_only_when_they_change() {
        let first = RawSettings {
            ems: Some(vec![0, 0xCC, 0]),
            backup_reserve: Some(vec![10]),
            running_state: Some(vec![0]),
            ..RawSettings::default()
        }
        .decode(100);
        // The first read is always saved.
        assert_eq!(settings_to_save(None, &first), Some(first.clone()));

        // Same values at a later time: nothing to save.
        let later = Settings {
            ts: 160,
            ..first.clone()
        };
        assert_eq!(settings_to_save(Some(&first), &later), None);

        // A refused register keeps the old value and is no change.
        let refused = Settings {
            ts: 160,
            backup_reserve_pct: None,
            running_state: None,
            ..first.clone()
        };
        assert_eq!(settings_to_save(Some(&first), &refused), None);

        // Running <-> Dispatch running alone is not a change.
        let dispatch = Settings {
            ts: 280,
            running_state: Some(0x8200),
            ..first.clone()
        };
        assert_eq!(settings_to_save(Some(&first), &dispatch), None);
        // Running -> Fault is.
        let fault = Settings {
            ts: 280,
            running_state: Some(0x5500),
            ..first.clone()
        };
        assert!(settings_to_save(Some(&first), &fault).is_some());

        // A real change is saved with the new running state, and a refused
        // field carries the old value.
        let changed = Settings {
            ts: 220,
            ems_mode: Some(4),
            running_state: Some(0x8200),
            backup_reserve_pct: None,
            ..first.clone()
        };
        let row = settings_to_save(Some(&first), &changed).unwrap();
        assert_eq!(row.ts, 220);
        assert_eq!(row.running_state, Some(0x8200));
        assert_eq!(row.backup_reserve_pct, Some(10));
    }
}
