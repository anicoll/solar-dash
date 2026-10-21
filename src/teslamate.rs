//! The Tesla's live charge state, read from TeslaMate over MQTT.
//!
//! TeslaMate publishes one retained topic per value, such as
//! `teslamate/cars/1/charger_power`, with a plain-text payload. We subscribe
//! to all of the car's topics, keep the ones we know in a [`CarState`] and
//! share the newest state with the web server through [`Car`].
//!
//! [`apply`] and [`Recorder`] are pure (no network, no database) so they can
//! be unit tested. [`run`] does the network part. The car's state is not
//! stored: the topics are retained, so the broker hands us the latest values
//! again after every restart. Only charging sessions go in the database, a
//! few writes per charge (see [`Recorder`]).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::Serialize;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::config::Teslamate;
use crate::db::{self, ChargeProgress};

/// How often we ping the broker to show we are still here.
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// First pause after a connection error. It doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// While charging, the session row is updated at most this often. The row
/// lives on an SSD, so we keep writes rare.
const SAVE_EVERY_SECS: i64 = 5 * 60;

/// What we know about the car. A field is `None` when TeslaMate has not sent
/// it yet, sent an empty value, or sent something we could not read.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CarState {
    /// The name given to the car in the Tesla app.
    pub display_name: Option<String>,
    /// What the car is doing: "online", "asleep", "charging", "driving"...
    pub state: Option<String>,
    /// Whether a charging cable is plugged in.
    pub plugged_in: Option<bool>,
    /// "Charging", "Complete", "Stopped", "Disconnected"...
    pub charging_state: Option<String>,
    /// Power going into the car, kW.
    pub charger_power_kw: Option<f64>,
    /// Charging current, A.
    pub charger_actual_current_a: Option<f64>,
    /// Charging voltage, V.
    pub charger_voltage_v: Option<f64>,
    /// Battery level, 0 to 100.
    pub battery_level_pct: Option<f64>,
    /// Battery level that can actually be used, 0 to 100.
    pub usable_battery_level_pct: Option<f64>,
    /// Energy added in the current charging session, kWh.
    pub charge_energy_added_kwh: Option<f64>,
    /// The level the car stops charging at, 0 to 100.
    pub charge_limit_soc_pct: Option<f64>,
    /// Hours until the battery is full.
    pub time_to_full_charge_h: Option<f64>,
    /// The named place the car is at (for example "Home"), if it is at one.
    pub geofence: Option<String>,
    /// Unix seconds, UTC, when the last message was applied.
    pub updated_ts: i64,
}

/// The newest car state, shared between the MQTT task and the web server so
/// `/api/live` can answer without any lookup. Cloning gives another handle
/// to the same slot. It stays `None` until the first message arrives.
#[derive(Clone, Default)]
pub struct Car(Arc<Mutex<Option<CarState>>>);

impl Car {
    pub fn set(&self, state: CarState) {
        // A poisoned lock only means a panic elsewhere; the value is still fine.
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
    }

    pub fn get(&self) -> Option<CarState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// An empty payload means "unknown"; anything else is the text itself.
fn parse_text(payload: &str) -> Option<String> {
    (!payload.is_empty()).then(|| payload.to_string())
}

/// A number, or `None` for an empty or unreadable payload (the latter is
/// logged at debug level, since a bad value is not worth a warning).
fn parse_number(name: &str, payload: &str) -> Option<f64> {
    if payload.is_empty() {
        return None;
    }
    // "NaN" and "inf" parse as floats, but they are not measurements (and
    // JSON cannot hold them).
    match payload.parse::<f64>() {
        Ok(value) if value.is_finite() => Some(value),
        _ => {
            debug!(payload, "ignoring unreadable number for {name}");
            None
        }
    }
}

/// `true` or `false`, or `None` for an empty or unreadable payload.
fn parse_bool(name: &str, payload: &str) -> Option<bool> {
    match payload {
        "" => None,
        "true" => Some(true),
        "false" => Some(false),
        _ => {
            debug!(payload, "ignoring unreadable true/false for {name}");
            None
        }
    }
}

/// Stores one topic's payload in `state`. `name` is the last part of the
/// topic, such as `charger_power`. Returns `false` (and changes nothing) if
/// we do not use that topic.
pub fn apply(state: &mut CarState, name: &str, payload: &str) -> bool {
    match name {
        "display_name" => state.display_name = parse_text(payload),
        "state" => state.state = parse_text(payload),
        "plugged_in" => state.plugged_in = parse_bool(name, payload),
        "charging_state" => state.charging_state = parse_text(payload),
        "charger_power" => state.charger_power_kw = parse_number(name, payload),
        "charger_actual_current" => state.charger_actual_current_a = parse_number(name, payload),
        "charger_voltage" => state.charger_voltage_v = parse_number(name, payload),
        "battery_level" => state.battery_level_pct = parse_number(name, payload),
        "usable_battery_level" => state.usable_battery_level_pct = parse_number(name, payload),
        "charge_energy_added" => state.charge_energy_added_kwh = parse_number(name, payload),
        "charge_limit_soc" => state.charge_limit_soc_pct = parse_number(name, payload),
        "time_to_full_charge" => state.time_to_full_charge_h = parse_number(name, payload),
        "geofence" => state.geofence = parse_text(payload),
        _ => return false,
    }
    true
}

/// Handles one MQTT message. `base` is the start of the car's topics,
/// including the final slash (for example `teslamate/cars/1/`). Messages
/// for topics we do not use leave the shared state alone.
fn handle_publish(car: &Car, base: &str, topic: &str, payload: &[u8]) {
    let Some(name) = topic.strip_prefix(base) else {
        return;
    };
    let Ok(payload) = std::str::from_utf8(payload) else {
        debug!("ignoring non-text payload for {name}");
        return;
    };
    let mut state = car.get().unwrap_or_default();
    if apply(&mut state, name, payload.trim()) {
        state.updated_ts = db::now();
        car.set(state);
    }
}

/// What to write to the database after a car message.
#[derive(Debug, PartialEq)]
enum Step {
    /// Charging has begun: add a row.
    Start,
    /// Still charging: save the latest values.
    Save(i64, ChargeProgress),
    /// Charging has stopped: save the final values and close the row.
    End(i64, ChargeProgress),
}

/// A session we are keeping up to date.
struct Open {
    id: i64,
    /// The newest values seen, saved or not.
    progress: ChargeProgress,
    /// Unix seconds of the last save.
    saved_at: i64,
}

/// Turns the car's messages into charging sessions. It only decides what to
/// write (a [`Step`]) and leaves the writing to [`run`], so it can be unit
/// tested. It starts a session when `charging_state` becomes "Charging" and
/// ends it when it becomes anything else. Until `charging_state` has arrived
/// it does nothing, because after a restart the topics come in one by one.
struct Recorder {
    open: Option<Open>,
}

impl Recorder {
    /// A recorder with no session. Pass the open one from the database, if
    /// there is one, so a restart carries on with it instead of adding a row.
    fn new(open: Option<db::OpenCharge>, now: i64) -> Self {
        Self {
            open: open.map(|open| Open {
                id: open.id,
                progress: open.progress,
                saved_at: now,
            }),
        }
    }

    /// Looks at the car's newest state at time `now` and says what to write.
    fn next(&mut self, state: &CarState, now: i64) -> Option<Step> {
        let charging = state.charging_state.as_deref()? == "Charging";
        match (&mut self.open, charging) {
            (None, true) => Some(Step::Start),
            (None, false) => None,
            (Some(open), true) => {
                open.progress.add(state);
                if now - open.saved_at < SAVE_EVERY_SECS {
                    return None;
                }
                open.saved_at = now;
                Some(Step::Save(open.id, open.progress.clone()))
            }
            // The values from the last message while charging are the final
            // ones: the car may clear them once it stops.
            (Some(_), false) => self
                .open
                .take()
                .map(|open| Step::End(open.id, open.progress)),
        }
    }

    /// Call after a [`Step::Start`] was written, with the new row's id.
    fn started(&mut self, id: i64, state: &CarState, now: i64) {
        // The energy is left out: until the car sends a new value it still
        // holds the one from the last session.
        let mut progress = ChargeProgress::default();
        progress.add(state);
        progress.energy_added_kwh = None;
        self.open = Some(Open {
            id,
            progress,
            saved_at: now,
        });
    }
}

impl ChargeProgress {
    /// Takes the car's newest values: the highest power wins, and a value
    /// the car does not have leaves the old one.
    fn add(&mut self, state: &CarState) {
        self.energy_added_kwh = state.charge_energy_added_kwh.or(self.energy_added_kwh);
        self.soc_pct = state.battery_level_pct.or(self.soc_pct);
        self.max_power_kw = match (self.max_power_kw, state.charger_power_kw) {
            (Some(old), Some(new)) => Some(old.max(new)),
            (old, new) => old.or(new),
        };
    }
}

/// Writes what the recorder decided. A database error is logged and the
/// car keeps being followed: losing a session row is better than stopping.
async fn record(
    pool: &SqlitePool,
    recorder: &mut Recorder,
    car_id: i64,
    state: &CarState,
    now: i64,
) {
    let result = match recorder.next(state, now) {
        None => return,
        Some(Step::Start) => {
            let geofence = state.geofence.as_deref();
            db::start_charge_session(pool, car_id, now, state.battery_level_pct, geofence)
                .await
                .map(|id| recorder.started(id, state, now))
        }
        Some(Step::Save(id, progress)) => db::update_charge_session(pool, id, &progress).await,
        Some(Step::End(id, progress)) => db::end_charge_session(pool, id, now, &progress).await,
    };
    if let Err(err) = result {
        warn!("could not save the charging session: {err:#}");
    }
}

/// The pause after each failed connection attempt: it doubles up to a
/// limit, and starts over once a connection works. A plain struct with no
/// sleeping of its own, so it can be unit tested.
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self { next: MIN_BACKOFF }
    }

    /// How long to wait now. The one after it is twice as long, up to the limit.
    fn pause(&mut self) -> Duration {
        let now = self.next;
        self.next = (self.next * 2).min(MAX_BACKOFF);
        now
    }

    /// Call when the connection works again.
    fn reset(&mut self) {
        self.next = MIN_BACKOFF;
    }
}

/// Follows the car's topics on the broker and keeps `car` up to date, and
/// records its charging sessions in the database, until `shutdown` completes. Connection errors are logged (the first one, and
/// when it recovers) and never end the loop: TeslaMate may be down, and the
/// rest of `serve` must keep going without it.
pub async fn run(
    config: Teslamate,
    car: Car,
    pool: SqlitePool,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut options = MqttOptions::new("solar-dash", &config.host, config.port);
    options.set_keep_alive(KEEP_ALIVE);
    options.set_clean_session(true);
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        options.set_credentials(username, password);
    }
    // The second number is how many requests can wait for the event loop.
    let (client, mut eventloop) = AsyncClient::new(options, 16);

    let base = format!("{}/cars/{}/", config.topic_prefix, config.car_id);
    let filter = format!("{base}+");
    let mut backoff = Backoff::new();
    // A session left open by the last run is carried on or closed once the
    // retained topics say whether the car is still charging.
    let car_id = i64::from(config.car_id);
    let open = db::open_charge_session(&pool, car_id)
        .await
        .unwrap_or_else(|err| {
            warn!("could not read the open charging session: {err:#}");
            None
        });
    let mut recorder = Recorder::new(open, db::now());
    // Whether the last attempt failed, so the log gets one line per outage.
    let mut failing = false;

    tokio::pin!(shutdown);
    info!(broker = %config.host, "TeslaMate MQTT started");
    loop {
        // Dropping an unfinished poll on Ctrl+C is fine; nothing is half-saved.
        let event = tokio::select! {
            _ = &mut shutdown => break,
            event = eventloop.poll() => event,
        };
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                if failing {
                    info!("TeslaMate MQTT back");
                    failing = false;
                }
                backoff.reset();
                // With a clean session the broker forgets our subscription
                // when the connection drops, so subscribe after every connect.
                if let Err(err) = client.subscribe(&filter, QoS::AtMostOnce).await {
                    warn!("could not subscribe to {filter}: {err:#}");
                }
            }
            Ok(Event::Incoming(Packet::Publish(message))) => {
                handle_publish(&car, &base, &message.topic, &message.payload);
                if let Some(state) = car.get() {
                    record(&pool, &mut recorder, car_id, &state, db::now()).await;
                }
            }
            Ok(_) => {}
            Err(err) => {
                if !failing {
                    warn!("TeslaMate MQTT connection failed: {err:#}");
                    failing = true;
                }
                // The next `poll` reconnects by itself, after this pause.
                tokio::select! {
                    _ = &mut shutdown => break,
                    _ = tokio::time::sleep(backoff.pause()) => {}
                }
            }
        }
    }
    info!("TeslaMate MQTT stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(name: &str, payload: &str) -> CarState {
        let mut state = CarState::default();
        assert!(apply(&mut state, name, payload), "{name} should be known");
        state
    }

    #[test]
    fn numbers_are_parsed() {
        assert_eq!(applied("charger_power", "7.2").charger_power_kw, Some(7.2));
        assert_eq!(
            applied("charger_actual_current", "32").charger_actual_current_a,
            Some(32.0)
        );
        assert_eq!(
            applied("charger_voltage", "241").charger_voltage_v,
            Some(241.0)
        );
        assert_eq!(applied("battery_level", "43").battery_level_pct, Some(43.0));
        assert_eq!(
            applied("usable_battery_level", "42").usable_battery_level_pct,
            Some(42.0)
        );
        assert_eq!(
            applied("charge_energy_added", "12.34").charge_energy_added_kwh,
            Some(12.34)
        );
        assert_eq!(
            applied("charge_limit_soc", "80").charge_limit_soc_pct,
            Some(80.0)
        );
        assert_eq!(
            applied("time_to_full_charge", "1.5").time_to_full_charge_h,
            Some(1.5)
        );
        assert_eq!(applied("charger_power", "0").charger_power_kw, Some(0.0));
        assert_eq!(
            applied("charger_power", "-1.5").charger_power_kw,
            Some(-1.5)
        );
    }

    #[test]
    fn text_is_kept_as_it_is() {
        assert_eq!(
            applied("display_name", "Tessie").display_name.as_deref(),
            Some("Tessie")
        );
        assert_eq!(
            applied("state", "charging").state.as_deref(),
            Some("charging")
        );
        assert_eq!(
            applied("charging_state", "Complete")
                .charging_state
                .as_deref(),
            Some("Complete")
        );
        assert_eq!(
            applied("geofence", "Home").geofence.as_deref(),
            Some("Home")
        );
    }

    #[test]
    fn booleans_are_parsed() {
        assert_eq!(applied("plugged_in", "true").plugged_in, Some(true));
        assert_eq!(applied("plugged_in", "false").plugged_in, Some(false));
    }

    #[test]
    fn empty_payload_clears_the_value() {
        let mut state = CarState {
            charger_power_kw: Some(7.2),
            plugged_in: Some(true),
            geofence: Some("Home".to_string()),
            ..CarState::default()
        };
        assert!(apply(&mut state, "charger_power", ""));
        assert!(apply(&mut state, "plugged_in", ""));
        assert!(apply(&mut state, "geofence", ""));
        assert_eq!(state, CarState::default());
    }

    #[test]
    fn garbage_becomes_none() {
        let mut state = CarState {
            battery_level_pct: Some(50.0),
            plugged_in: Some(true),
            ..CarState::default()
        };
        // The topic is known, so `apply` says so, but the old value goes.
        assert!(apply(&mut state, "battery_level", "lots"));
        assert!(apply(&mut state, "plugged_in", "maybe"));
        assert_eq!(state.battery_level_pct, None);
        assert_eq!(state.plugged_in, None);
        // Floats that are not measurements.
        assert_eq!(applied("charger_power", "NaN").charger_power_kw, None);
        assert_eq!(applied("charger_power", "inf").charger_power_kw, None);
        // True/false are exact words.
        assert_eq!(applied("plugged_in", "True").plugged_in, None);
    }

    #[test]
    fn unknown_topic_changes_nothing() {
        let mut state = CarState::default();
        assert!(!apply(&mut state, "odometer", "12345"));
        assert!(!apply(&mut state, "", "x"));
        assert!(!apply(&mut state, "charger_power_extra", "1"));
        assert_eq!(state, CarState::default());
    }

    #[test]
    fn other_fields_are_left_alone() {
        let mut state = applied("battery_level", "43");
        assert!(apply(&mut state, "charger_power", "7.2"));
        assert_eq!(state.battery_level_pct, Some(43.0));
        assert_eq!(state.charger_power_kw, Some(7.2));
    }

    #[test]
    fn car_starts_empty_and_shares_one_slot() {
        let car = Car::default();
        assert_eq!(car.get(), None);
        let other = car.clone();
        other.set(CarState {
            updated_ts: 5,
            ..CarState::default()
        });
        assert_eq!(car.get().unwrap().updated_ts, 5);
    }

    #[test]
    fn publish_updates_the_shared_state() {
        let car = Car::default();
        let base = "teslamate/cars/1/";
        handle_publish(&car, base, "teslamate/cars/1/battery_level", b"43");
        handle_publish(&car, base, "teslamate/cars/1/plugged_in", b"true\n");
        let state = car.get().unwrap();
        assert_eq!(state.battery_level_pct, Some(43.0));
        assert_eq!(state.plugged_in, Some(true));
        assert!(state.updated_ts > 1_700_000_000);
    }

    #[test]
    fn publish_for_unknown_or_other_topics_is_ignored() {
        let car = Car::default();
        let base = "teslamate/cars/1/";
        handle_publish(&car, base, "teslamate/cars/1/odometer", b"1");
        // Another car, and a different namespace.
        handle_publish(&car, base, "teslamate/cars/2/battery_level", b"43");
        handle_publish(&car, base, "other/cars/1/battery_level", b"43");
        // Not text at all.
        handle_publish(&car, base, "teslamate/cars/1/battery_level", &[0xFF, 0xFE]);
        assert_eq!(car.get(), None);
    }

    fn charging(state: &str, power: f64) -> CarState {
        CarState {
            charging_state: Some(state.to_string()),
            charger_power_kw: Some(power),
            charge_energy_added_kwh: Some(1.0),
            battery_level_pct: Some(50.0),
            ..CarState::default()
        }
    }

    #[test]
    fn session_starts_when_charging_begins() {
        let mut recorder = Recorder::new(None, 0);
        // Nothing is known yet, then not charging.
        assert_eq!(recorder.next(&CarState::default(), 100), None);
        assert_eq!(recorder.next(&charging("Disconnected", 0.0), 110), None);
        assert_eq!(
            recorder.next(&charging("Charging", 7.0), 120),
            Some(Step::Start)
        );
        recorder.started(5, &charging("Charging", 7.0), 120);
        // Still charging, and saved a moment ago: nothing to write.
        assert_eq!(recorder.next(&charging("Charging", 7.2), 130), None);
    }

    #[test]
    fn first_message_charging_starts_a_session() {
        // After a restart with no open row, "Charging" with no earlier state.
        let mut recorder = Recorder::new(None, 0);
        assert_eq!(
            recorder.next(&charging("Charging", 7.0), 1),
            Some(Step::Start)
        );
    }

    #[test]
    fn progress_is_saved_every_five_minutes_with_the_highest_power() {
        let mut recorder = Recorder::new(None, 0);
        recorder.started(5, &charging("Charging", 3.0), 1000);
        assert_eq!(recorder.next(&charging("Charging", 7.4), 1100), None);
        assert_eq!(recorder.next(&charging("Charging", 6.0), 1299), None);
        let mut later = charging("Charging", 6.0);
        later.charge_energy_added_kwh = Some(2.5);
        later.battery_level_pct = Some(52.0);
        let expected = ChargeProgress {
            energy_added_kwh: Some(2.5),
            soc_pct: Some(52.0),
            max_power_kw: Some(7.4),
        };
        assert_eq!(recorder.next(&later, 1300), Some(Step::Save(5, expected)));
        // The clock restarts after a save.
        assert_eq!(recorder.next(&later, 1400), None);
        assert!(matches!(
            recorder.next(&later, 1600),
            Some(Step::Save(5, _))
        ));
    }

    #[test]
    fn session_ends_when_charging_stops_with_the_last_values() {
        let mut recorder = Recorder::new(None, 0);
        recorder.started(5, &charging("Charging", 3.0), 1000);
        recorder.next(&charging("Charging", 7.0), 1010);
        // The car clears its values as it stops; the last ones are kept.
        let stopped = CarState {
            charging_state: Some("Complete".to_string()),
            ..CarState::default()
        };
        let expected = ChargeProgress {
            energy_added_kwh: Some(1.0),
            soc_pct: Some(50.0),
            max_power_kw: Some(7.0),
        };
        assert_eq!(recorder.next(&stopped, 1020), Some(Step::End(5, expected)));
        // It is over: nothing more, until charging begins again.
        assert_eq!(recorder.next(&stopped, 1030), None);
        assert_eq!(
            recorder.next(&charging("Charging", 7.0), 1040),
            Some(Step::Start)
        );
    }

    #[test]
    fn a_new_session_ignores_the_old_energy_value() {
        let mut recorder = Recorder::new(None, 0);
        recorder.started(5, &charging("Charging", 3.0), 1000);
        let expected = ChargeProgress {
            energy_added_kwh: None,
            soc_pct: Some(50.0),
            max_power_kw: Some(3.0),
        };
        let stopped = CarState {
            charging_state: Some("Stopped".to_string()),
            battery_level_pct: Some(50.0),
            ..CarState::default()
        };
        assert_eq!(recorder.next(&stopped, 1001), Some(Step::End(5, expected)));
    }

    #[test]
    fn restart_while_still_charging_carries_on_with_the_open_row() {
        let open = db::OpenCharge {
            id: 9,
            progress: ChargeProgress {
                energy_added_kwh: Some(4.0),
                soc_pct: Some(48.0),
                max_power_kw: Some(7.4),
            },
        };
        let mut recorder = Recorder::new(Some(open), 2000);
        // The retained topics arrive one by one: no charging_state yet.
        assert_eq!(recorder.next(&CarState::default(), 2001), None);
        // No new row; this save happens at the next 5 minute mark.
        assert_eq!(recorder.next(&charging("Charging", 7.0), 2002), None);
        let step = recorder.next(&charging("Charging", 7.0), 2300);
        let expected = ChargeProgress {
            energy_added_kwh: Some(1.0),
            soc_pct: Some(50.0),
            // The highest power from before the restart is kept.
            max_power_kw: Some(7.4),
        };
        assert_eq!(step, Some(Step::Save(9, expected)));
    }

    #[test]
    fn restart_after_charging_ended_closes_the_open_row() {
        let open = db::OpenCharge {
            id: 9,
            progress: ChargeProgress {
                energy_added_kwh: Some(4.0),
                soc_pct: Some(48.0),
                max_power_kw: Some(7.4),
            },
        };
        let mut recorder = Recorder::new(Some(open.clone()), 2000);
        assert_eq!(recorder.next(&CarState::default(), 2001), None);
        let state = CarState {
            charging_state: Some("Disconnected".to_string()),
            ..CarState::default()
        };
        assert_eq!(
            recorder.next(&state, 2002),
            Some(Step::End(9, open.progress))
        );
    }

    #[test]
    fn backoff_doubles_up_to_the_limit_and_resets() {
        let mut backoff = Backoff::new();
        let secs: Vec<u64> = (0..9).map(|_| backoff.pause().as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        backoff.reset();
        assert_eq!(backoff.pause().as_secs(), 1);
    }
}
