//! Home Assistant as a **source**: reads the states of the entities named in
//! `[[home_assistant.source.entity]]` over Home Assistant's WebSocket API.
//!
//! **Read-only.** The only things sent are the login (`auth`), a subscription
//! to state changes and one `get_states`; solar-dash never calls a service
//! and never writes a state in Home Assistant (see [`protocol`]).
//!
//! What is stored:
//!
//! - every numeric state goes into `ha_reading` (at most one row per entity
//!   per [`MIN_WRITE_SECS`]; the newest value in between is kept and written
//!   when the time is up),
//! - an entity with a `metric` is also stored in `metric_reading` under the
//!   source name [`SOURCE_NAME`], converted to the metric's unit,
//! - the last state (text too) and when it changed stay in memory for
//!   `GET /api/ha/entities` (see [`HaView`]).
//!
//! **Design rule:** anything that came from Home Assistant is never sent
//! back to it. The MQTT sink skips events from [`SOURCE_NAME`] (see
//! [`is_home_assistant_source`] and `home_assistant::Feed::on_event`).
//!
//! Past values come from Home Assistant's long-term statistics, once per
//! entity (see [`backfill`]).
//!
//! The split follows the MQTT sink: [`protocol`] and [`Ingest`] are plain
//! code tested without a network; [`run`] is the thin loop that connects,
//! reconnects and moves data between them and the database.

pub mod backfill;
pub mod protocol;
pub mod samples;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use self::protocol::{Session, StateUpdate, Step};
use crate::config::{HaEntity, HaSource};
use crate::hub::{Hub, HubEvent};
use crate::metric::Metric;
use crate::teslamate::Backoff;

/// The name readings are stored under in `metric_reading`, and the id of
/// the spoke.
pub const SOURCE_NAME: &str = "home-assistant";

/// At most one stored row per entity per this many seconds.
pub const MIN_WRITE_SECS: i64 = 15;

/// How often the throttle is asked for values that have waited long enough.
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Give up on a connection attempt after this long.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Home Assistant pings about every minute; no message at all for this long
/// means the connection is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);

/// Whether `source` (a `metric_reading` or event source name) is Home
/// Assistant's. The MQTT sink uses this to never publish such data back.
pub fn is_home_assistant_source(source: &str) -> bool {
    source == SOURCE_NAME
}

/// One numeric reading, ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub entity_id: String,
    pub ts: i64,
    /// As Home Assistant reported it.
    pub value: f64,
    pub unit: Option<String>,
    /// The metric and the value in the metric's unit, when the entity is
    /// mapped and its unit could be converted.
    pub metric: Option<(Metric, f64)>,
}

/// `value` in `unit` as the metric's own unit (W, Wh or %), or `None` when
/// the unit is missing or does not fit the metric (kWh for a power metric,
/// say). Guessing would put wrong numbers in the dashboard.
pub fn to_metric_unit(metric: Metric, unit: Option<&str>, value: f64) -> Option<f64> {
    let factor = match (metric.unit(), unit?.trim()) {
        ("W", "W") | ("Wh", "Wh") | ("%", "%") => 1.0,
        ("W", "kW") | ("Wh", "kWh") => 1_000.0,
        ("W", "MW") | ("Wh", "MWh") => 1_000_000.0,
        _ => return None,
    };
    Some(value * factor)
}

/// What we know about one entity right now, for `/api/ha/entities`.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityState {
    /// The last state text, numeric or not ("412.5", "unavailable").
    pub state: String,
    /// The last numeric value, if the last state was one.
    pub value: Option<f64>,
    pub unit: Option<String>,
    /// When Home Assistant last changed it, unix seconds.
    pub last_updated: i64,
}

/// What the web API can see of the source. Cloning shares the same data
/// (the spoke writes, the web handlers read).
#[derive(Clone)]
pub struct HaView {
    entities: Arc<[HaEntity]>,
    connected: Arc<AtomicBool>,
    states: Arc<Mutex<HashMap<String, EntityState>>>,
}

impl HaView {
    pub fn new(entities: &[HaEntity]) -> Self {
        Self {
            entities: entities.into(),
            connected: Arc::new(AtomicBool::new(false)),
            states: Arc::default(),
        }
    }

    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    fn set_state(&self, entity_id: &str, state: EntityState) {
        // A poisoned lock only means another thread panicked mid-update of a
        // plain map; the data is still usable.
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states.insert(entity_id.to_string(), state);
    }

    /// The JSON of `GET /api/ha/entities`.
    pub fn to_json(&self) -> Value {
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let entities: Vec<Value> = self
            .entities
            .iter()
            .map(|entity| {
                let state = states.get(&entity.entity_id);
                json!({
                    "entity_id": entity.entity_id,
                    "name": entity.name.as_deref().unwrap_or(&entity.entity_id),
                    "metric": entity.metric.map(Metric::name),
                    "state": state.map(|s| s.state.as_str()),
                    "value": state.and_then(|s| s.value),
                    "unit": state
                        .and_then(|s| s.unit.as_deref())
                        .or(entity.unit.as_deref()),
                    "last_updated": state.map(|s| s.last_updated),
                })
            })
            .collect();
        json!({
            "configured": true,
            "connected": self.connected.load(Ordering::Relaxed),
            "entities": entities,
        })
    }
}

/// Keeps the rate of stored rows down: one per entity per
/// [`MIN_WRITE_SECS`], and the newest value is never lost, only delayed.
#[derive(Default)]
struct Throttle {
    slots: HashMap<String, Slot>,
}

#[derive(Default)]
struct Slot {
    /// Time of the last row let through.
    last_ts: Option<i64>,
    /// A newer reading that arrived too soon.
    held: Option<Reading>,
}

impl Throttle {
    /// A new reading: `Some` if it may be stored now, else it is held (and
    /// replaces any older held one).
    fn offer(&mut self, reading: Reading) -> Option<Reading> {
        let slot = self.slots.entry(reading.entity_id.clone()).or_default();
        // `ts < last` too: a clock that stepped back must not freeze the entity.
        let due = slot
            .last_ts
            .is_none_or(|last| reading.ts < last || reading.ts - last >= MIN_WRITE_SECS);
        if due {
            slot.last_ts = Some(reading.ts);
            slot.held = None;
            Some(reading)
        } else {
            slot.held = Some(reading);
            None
        }
    }

    /// Held readings whose time has come at `now`. With `force`, all of them
    /// (on disconnect and shutdown, so nothing waits in memory).
    fn flush(&mut self, now: i64, force: bool) -> Vec<Reading> {
        let mut out = Vec::new();
        for slot in self.slots.values_mut() {
            let ready = force || slot.last_ts.is_none_or(|last| now - last >= MIN_WRITE_SECS);
            if ready && let Some(held) = slot.held.take() {
                slot.last_ts = Some(held.ts);
                out.push(held);
            }
        }
        out.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
        out
    }
}

/// Turns Home Assistant's states into rows to store: keeps the live view up
/// to date, drops non-numeric states, maps units and throttles. No I/O.
pub struct Ingest {
    entities: HashMap<String, HaEntity>,
    throttle: Throttle,
    view: HaView,
    /// Entities already warned about for a unit that does not fit their
    /// metric, so the log has one line each, not one per update.
    warned: HashSet<String>,
}

impl Ingest {
    pub fn new(entities: &[HaEntity], view: HaView) -> Self {
        Self {
            entities: entities
                .iter()
                .map(|entity| (entity.entity_id.clone(), entity.clone()))
                .collect(),
            throttle: Throttle::default(),
            view,
            warned: HashSet::new(),
        }
    }

    /// Handles updates and returns the rows to store now.
    pub fn on_updates(&mut self, updates: Vec<StateUpdate>) -> Vec<Reading> {
        let mut out = Vec::new();
        for update in updates {
            let Some(entity) = self.entities.get(&update.entity_id) else {
                continue;
            };
            // The config's unit wins over what Home Assistant says.
            let unit = entity.unit.clone().or_else(|| update.unit.clone());
            let value = update.numeric();
            self.view.set_state(
                &update.entity_id,
                EntityState {
                    state: update.state.clone(),
                    value,
                    unit: unit.clone(),
                    last_updated: update.ts,
                },
            );
            let Some(value) = value else { continue };
            let metric = entity.metric.and_then(|metric| {
                let converted = to_metric_unit(metric, unit.as_deref(), value);
                if converted.is_none() && self.warned.insert(update.entity_id.clone()) {
                    warn!(
                        entity = %update.entity_id,
                        metric = metric.name(),
                        unit = unit.as_deref().unwrap_or("none"),
                        "Home Assistant unit does not fit the metric ({}); \
                         not stored as that metric (set `unit` on the entity if Home Assistant reports none)",
                        metric.unit()
                    );
                }
                converted.map(|converted| (metric, converted))
            });
            let reading = Reading {
                entity_id: update.entity_id,
                ts: update.ts,
                value,
                unit,
                metric,
            };
            out.extend(self.throttle.offer(reading));
        }
        out
    }

    /// Rows that were held back and are now due (see [`Throttle::flush`]).
    pub fn flush(&mut self, now: i64, force: bool) -> Vec<Reading> {
        self.throttle.flush(now, force)
    }
}

/// Logs once per outage and paces the retries.
struct Health {
    backoff: Backoff,
    /// Whether this outage has been logged already.
    logged: bool,
}

impl Health {
    fn new() -> Self {
        Self {
            backoff: Backoff::new(),
            logged: false,
        }
    }

    /// A good connection: log the recovery if an outage was logged.
    fn connected(&mut self) {
        if self.logged {
            info!("Home Assistant is reachable again");
        }
        self.logged = false;
        self.backoff.reset();
    }

    /// A failed connection. Returns how long to wait before the next try;
    /// logs only the first failure of an outage.
    fn failed(&mut self, err: &anyhow::Error) -> Duration {
        if !self.logged {
            warn!("Home Assistant source: {err:#}; retrying with a growing pause");
            self.logged = true;
        } else {
            debug!("Home Assistant source still failing: {err:#}");
        }
        self.backoff.pause()
    }
}

/// Reads Home Assistant until `shutdown` completes. Trouble is logged once
/// per outage and retried with a growing pause; it never ends the loop and
/// never blocks `serve`.
pub async fn run(
    config: HaSource,
    view: HaView,
    pool: SqlitePool,
    hub: Hub,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let url = config.websocket_url()?;
    info!(
        entities = config.entity.len(),
        "Home Assistant source started (read-only)"
    );
    // Show the last stored values at once, before the first connection.
    for entity in &config.entity {
        if let Some((ts, value, unit)) = store::latest(&pool, &entity.entity_id).await? {
            view.set_state(
                &entity.entity_id,
                EntityState {
                    state: value.to_string(),
                    value: Some(value),
                    unit,
                    last_updated: ts,
                },
            );
        }
    }
    // The live reading and the history backfill run side by side on this
    // task, each with its own connection; shutdown stops both.
    tokio::select! {
        _ = shutdown => {}
        _ = async {
            tokio::join!(
                live_loop(&url, &config, &view, &pool, &hub),
                backfill::run(&url, &config, &pool, &hub),
            )
        } => {}
    }
    view.set_connected(false);
    Ok(())
}

/// Keeps the live connection going forever: connect, read, and on any
/// failure pause (logging once per outage) and connect again.
async fn live_loop(url: &str, config: &HaSource, view: &HaView, pool: &SqlitePool, hub: &Hub) {
    let mut ingest = Ingest::new(&config.entity, view.clone());
    let mut health = Health::new();
    loop {
        let result = connection(url, config, &mut ingest, pool, hub, &mut health).await;
        view.set_connected(false);
        // Do not leave a held reading waiting in memory.
        let held = ingest.flush(Utc::now().timestamp(), true);
        if let Err(err) = save_and_announce(pool, hub, held).await {
            warn!("could not save Home Assistant readings: {err:#}");
        }
        let err = match result {
            Ok(()) => anyhow::anyhow!("the connection was closed"),
            Err(err) => err,
        };
        tokio::time::sleep(health.failed(&err)).await;
    }
}

/// One connection, from connecting until it fails.
async fn connection(
    url: &str,
    config: &HaSource,
    ingest: &mut Ingest,
    pool: &SqlitePool,
    hub: &Hub,
    health: &mut Health,
) -> Result<()> {
    let (mut socket, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .context("timed out connecting to Home Assistant")?
        .context("could not connect to Home Assistant")?;
    let ids = config.entity.iter().map(|e| e.entity_id.clone()).collect();
    let mut session = Session::new(&config.token, ids);
    let mut was_authenticated = false;
    let mut flush_timer = interval(FLUSH_EVERY);
    loop {
        tokio::select! {
            message = timeout(IDLE_TIMEOUT, socket.next()) => {
                let message = message
                    .context("no message from Home Assistant for too long")?
                    .context("Home Assistant closed the connection")?
                    .context("Home Assistant connection error")?;
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(_) => anyhow::bail!("Home Assistant closed the connection"),
                    // Pings are answered by the library; binary is not used.
                    _ => continue,
                };
                let steps = session.on_text(&text, Utc::now().timestamp())?;
                if session.is_authenticated() && !was_authenticated {
                    was_authenticated = true;
                    health.connected();
                    ingest.view.set_connected(true);
                }
                for step in steps {
                    match step {
                        Step::Send(text) => socket
                            .send(Message::text(text))
                            .await
                            .context("could not send to Home Assistant")?,
                        Step::Updates(updates) => {
                            let rows = ingest.on_updates(updates);
                            save_and_announce(pool, hub, rows).await?;
                        }
                    }
                }
            }
            _ = flush_timer.tick() => {
                let rows = ingest.flush(Utc::now().timestamp(), false);
                save_and_announce(pool, hub, rows).await?;
            }
        }
    }
}

/// Stores rows and tells the hub about the metric ones. A database failure
/// ends the connection (and so is retried) instead of dropping readings
/// silently.
async fn save_and_announce(pool: &SqlitePool, hub: &Hub, rows: Vec<Reading>) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    store::save(pool, &rows).await?;
    let mapped: Vec<&Reading> = rows.iter().filter(|row| row.metric.is_some()).collect();
    if !mapped.is_empty() {
        let mut metrics: Vec<Metric> = mapped
            .iter()
            .filter_map(|r| r.metric)
            .map(|m| m.0)
            .collect();
        metrics.sort_by_key(|metric| metric.name());
        metrics.dedup();
        hub.publish(HubEvent::ReadingsStored {
            source: SOURCE_NAME.to_string(),
            metrics,
            from: mapped.iter().map(|r| r.ts).min().unwrap_or_default(),
            to: mapped.iter().map(|r| r.ts).max().unwrap_or_default(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
