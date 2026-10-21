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
use tokio_modbus::Slave;
use tokio_modbus::client::{Context as ModbusContext, Reader as _, tcp};
use tracing::{info, warn};

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

/// The inverter sends this for a 32-bit value it cannot measure right now.
const UNAVAILABLE: u32 = 0x7FFF_FFFF;

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

/// Solar power in W from registers 5016..=5017.
pub fn decode_pv(words: &[u16]) -> Option<f64> {
    decode_u32(words).map(f64::from)
}

/// Battery power in W from registers 5213..=5214, with our sign
/// (positive = charging).
pub fn decode_battery(words: &[u16]) -> Option<f64> {
    // Negate as an i64 so that 0 stays 0 (negating the float 0.0 gives -0.0,
    // which prints as "-0.00").
    decode_i32(words).map(|v| -i64::from(v) as f64)
}

/// Turns the 16-register block starting at 13007 into
/// `(load_w, grid_w, soc_pct)`.
pub fn decode_block(words: &[u16]) -> (Option<f64>, Option<f64>, Option<f64>) {
    // Offsets are the register number minus 13007.
    let load = decode_i32(&words[0..2]).map(f64::from);
    // Register: positive = exporting. We want positive = importing.
    let grid = decode_i32(&words[2..4]).map(|v| -i64::from(v) as f64);
    // 13022 is the last register of the block. 0.1 % per unit.
    let soc = Some(f64::from(words[15]) * 0.1);
    (load, grid, soc)
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

    /// Does the three reads and returns a [`Sample`]. On any error the
    /// connection is dropped, so calling this again reconnects.
    pub async fn read_sample(&mut self) -> Result<Sample> {
        let result = self.try_read().await;
        if result.is_err() {
            self.connection = None;
        }
        result
    }

    async fn try_read(&mut self) -> Result<Sample> {
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

        Ok(decode_sample(db::now(), &pv, &battery, &block))
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

/// Reads a sample every `interval_secs`, keeps it in `latest` and saves it,
/// until `shutdown` completes. Failures are logged (throttled) and never end the loop: the
/// dongle is flaky, and the rest of `serve` must keep going without it.
pub async fn run_poll(
    config: Modbus,
    pool: &SqlitePool,
    latest: Latest,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs));
    // After a slow read, don't make up for it with a burst of reads.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reader = Reader::new(config);
    let mut failures = FailureLog::default();

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
            Ok(sample) => {
                if let Some(count) = failures.succeeded() {
                    info!(failed_reads = count, "Modbus back");
                }
                latest.set(sample);
                if let Err(err) = db::insert_live_sample(pool, &sample).await {
                    warn!("could not save Modbus sample: {err:#}");
                }
            }
            Err(err) => {
                if failures.failed() {
                    warn!("Modbus read failed: {err:#}");
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

/// Reads `count` input registers (function 04) starting at `address`.
async fn read_registers(ctx: &mut ModbusContext, address: u16, count: u16) -> Result<Vec<u16>> {
    // tokio-modbus has two layers of result: the outer one is for transport
    // errors, the inner one for an error answer from the device.
    let reply = match timeout(REQUEST_TIMEOUT, ctx.read_input_registers(address, count)).await {
        Ok(reply) => reply,
        Err(_) => bail!("reading registers at {address} timed out"),
    };
    let words = reply
        .with_context(|| format!("reading registers at {address} failed"))?
        .map_err(|code| anyhow!("inverter refused registers at {address}: {code}"))?;
    if words.len() != usize::from(count) {
        bail!(
            "asked for {count} registers at {address}, got {}",
            words.len()
        );
    }
    Ok(words)
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

    // Real readings from the owner's inverter.
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
                Request::ReadInputRegisters(5016, 2) => Ok(PV.to_vec()),
                Request::ReadInputRegisters(5213, 2) => Ok(BATTERY.to_vec()),
                Request::ReadInputRegisters(13007, 16) => Ok(BLOCK.to_vec()),
                _ => Err(ExceptionCode::IllegalDataAddress),
            };
            ready(answer.map(Response::ReadInputRegisters))
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
        let sample = reader.read_sample().await.unwrap();
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
}
