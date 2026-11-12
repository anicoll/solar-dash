//! Publishes solar-dash's readings to Home Assistant over MQTT.
//!
//! Home Assistant can create sensors by itself from "MQTT discovery"
//! messages: for each sensor we publish one retained JSON config under
//! `<discovery_prefix>/sensor/<node_id>/<object_id>/config`, and then keep
//! sending the values to a state topic. Everything is grouped under one
//! Home Assistant device.
//!
//! This module is **publish only**. It never subscribes to a command topic
//! and never controls anything. The single thing it listens to is Home
//! Assistant's own birth message (`<discovery_prefix>/status` = "online"),
//! which means "I restarted, please send your configs again".
//!
//! The logic here is a plain state machine ([`Feed`]) that turns config and
//! hub events into [`Message`]s. The spoke in `hub/spokes/ha_mqtt.rs` only
//! moves those messages to the broker, so everything in here is tested
//! without a broker.
//!
//! Rule from the design: a metric that itself came *from* Home Assistant
//! must never be published back to it. There is no Home Assistant source
//! yet, so nothing needs skipping today; [`Feed::on_event`] is the one place
//! where such a source's readings would be filtered out.

use std::time::Duration;

use anyhow::Result;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::config::HomeAssistant;
use crate::hub::HubEvent;
use crate::modbus::Sample;
use crate::teslamate::Backoff;

const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// One MQTT message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub topic: String,
    pub payload: String,
    pub retain: bool,
}

/// Which state topic a sensor reads from. More topics are added next to
/// `Live` as more kinds of sensor appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateTopic {
    /// `<base_topic>/live`: one JSON message per inverter sample.
    Live,
}

impl StateTopic {
    fn suffix(self) -> &'static str {
        match self {
            StateTopic::Live => "live",
        }
    }
}

/// One sensor in Home Assistant.
#[derive(Debug, Clone, Copy)]
pub struct Sensor {
    /// Last part of the discovery topic and of the `unique_id`.
    pub object_id: &'static str,
    /// What Home Assistant shows, after the device name.
    pub name: &'static str,
    pub topic: StateTopic,
    /// The key in the state JSON that holds this sensor's value.
    pub field: &'static str,
    pub device_class: Option<&'static str>,
    pub state_class: &'static str,
    pub unit: &'static str,
    pub precision: u8,
}

/// The sensors made from the live inverter sample.
///
/// Signs follow `modbus::Sample`: grid is positive when importing, battery
/// is positive when charging. Import and export are also given as two
/// separate sensors that are never negative, which is what Home Assistant's
/// Energy dashboard and most graphs want.
pub const LIVE_SENSORS: [Sensor; 7] = [
    Sensor {
        object_id: "solar_power",
        name: "Solar power",
        topic: StateTopic::Live,
        field: "solar_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
    Sensor {
        object_id: "home_load",
        name: "Home load",
        topic: StateTopic::Live,
        field: "load_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
    Sensor {
        object_id: "grid_power",
        name: "Grid power",
        topic: StateTopic::Live,
        field: "grid_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
    Sensor {
        object_id: "battery_power",
        name: "Battery power",
        topic: StateTopic::Live,
        field: "battery_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
    Sensor {
        object_id: "battery_soc",
        name: "Battery state of charge",
        topic: StateTopic::Live,
        field: "battery_soc_pct",
        device_class: Some("battery"),
        state_class: "measurement",
        unit: "%",
        precision: 0,
    },
    Sensor {
        object_id: "grid_import",
        name: "Grid import",
        topic: StateTopic::Live,
        field: "grid_import_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
    Sensor {
        object_id: "grid_export",
        name: "Grid export",
        topic: StateTopic::Live,
        field: "grid_export_w",
        device_class: Some("power"),
        state_class: "measurement",
        unit: "W",
        precision: 0,
    },
];

/// Turns a device name into something safe to use as a topic level and as
/// the start of a `unique_id`: lower case letters, digits and `_`.
fn node_id(device_name: &str) -> String {
    let id: String = device_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if id.is_empty() {
        "solar_dash".into()
    } else {
        id
    }
}

/// Rounds to one decimal so the JSON is short and stable.
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Decides when to publish and what. Holds the throttle clock.
pub struct Feed {
    config: HomeAssistant,
    node_id: String,
    /// Sample time (unix seconds) of the last state message sent.
    last_state_ts: Option<i64>,
}

impl Feed {
    pub fn new(config: HomeAssistant) -> Self {
        let node_id = node_id(&config.device_name);
        Self {
            config,
            node_id,
            last_state_ts: None,
        }
    }

    pub fn status_topic(&self) -> String {
        format!("{}/status", self.config.base_topic)
    }

    /// Home Assistant's birth-message topic.
    pub fn ha_status_topic(&self) -> String {
        format!("{}/status", self.config.discovery_prefix)
    }

    fn state_topic(&self, topic: StateTopic) -> String {
        format!("{}/{}", self.config.base_topic, topic.suffix())
    }

    /// The discovery topic of one sensor.
    fn discovery_topic(&self, sensor: &Sensor) -> String {
        format!(
            "{}/sensor/{}/{}/config",
            self.config.discovery_prefix, self.node_id, sensor.object_id
        )
    }

    /// The retained discovery config of one sensor. All sensors share one
    /// `device` block, which is how Home Assistant groups them.
    pub fn discovery_message(&self, sensor: &Sensor) -> Message {
        let mut payload = json!({
            "name": sensor.name,
            "unique_id": format!("{}_{}", self.node_id, sensor.object_id),
            "state_topic": self.state_topic(sensor.topic),
            "value_template": format!("{{{{ value_json.{} }}}}", sensor.field),
            "availability_topic": self.status_topic(),
            "payload_available": "online",
            "payload_not_available": "offline",
            "state_class": sensor.state_class,
            "unit_of_measurement": sensor.unit,
            "suggested_display_precision": sensor.precision,
            "device": {
                "identifiers": [self.node_id],
                "name": self.config.device_name,
                "manufacturer": "solar-dash",
                "model": "solar-dash",
                "sw_version": env!("CARGO_PKG_VERSION"),
            },
        });
        if let Some(class) = sensor.device_class {
            payload["device_class"] = json!(class);
        }
        Message {
            topic: self.discovery_topic(sensor),
            payload: payload.to_string(),
            retain: true,
        }
    }

    /// Everything to send after (re)connecting: "online" and every
    /// discovery config. Also restarts the throttle so the first sample
    /// after a reconnect goes out at once.
    pub fn connect_messages(&mut self) -> Vec<Message> {
        self.last_state_ts = None;
        let mut out = vec![Message {
            topic: self.status_topic(),
            payload: "online".into(),
            retain: true,
        }];
        out.extend(LIVE_SENSORS.iter().map(|s| self.discovery_message(s)));
        out
    }

    /// Home Assistant says "online" when it (re)starts; it has forgotten the
    /// sensors, so send them again. Anything else on that topic is ignored.
    pub fn on_ha_status(&mut self, topic: &str, payload: &[u8]) -> Vec<Message> {
        if topic == self.ha_status_topic() && payload == b"online" {
            self.connect_messages()
        } else {
            Vec::new()
        }
    }

    /// The JSON state message for one sample.
    ///
    /// Signs: `grid_w` is positive when importing and negative when
    /// exporting; `battery_w` is positive when charging and negative when
    /// discharging. `grid_import_w` and `grid_export_w` split the grid figure
    /// into two values that are never negative. A value the inverter did not
    /// give is `null`.
    pub fn live_state(&self, sample: &Sample) -> Message {
        let state: Value = json!({
            "ts": sample.ts,
            "solar_w": sample.pv_w.map(round1),
            "load_w": sample.load_w.map(round1),
            "grid_w": sample.grid_w.map(round1),
            "battery_w": sample.battery_w.map(round1),
            "battery_soc_pct": sample.soc_pct.map(round1),
            "grid_import_w": sample.grid_w.map(|w| round1(w.max(0.0))),
            "grid_export_w": sample.grid_w.map(|w| round1((-w).max(0.0))),
        });
        Message {
            topic: self.state_topic(StateTopic::Live),
            payload: state.to_string(),
            // A state is only true for now; a retained one would show an old
            // reading as current after a restart.
            retain: false,
        }
    }

    /// What to send for one hub event.
    pub fn on_event(&mut self, event: &HubEvent) -> Vec<Message> {
        // A source that came from Home Assistant itself must be skipped here
        // so its values are never sent back. (None exists yet.)
        match event {
            HubEvent::SampleArrived(sample) => {
                let due = match self.last_state_ts {
                    None => true,
                    // `<` as well: a clock that stepped back must not freeze
                    // the feed.
                    Some(last) => {
                        sample.ts < last || sample.ts - last >= self.config.min_interval_secs as i64
                    }
                };
                if due {
                    self.last_state_ts = Some(sample.ts);
                    vec![self.live_state(sample)]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }
}

/// Hands a message to the MQTT client. `try_publish` never waits, so a full
/// queue drops the message instead of stalling the loop (we only send while
/// connected, so the queue stays nearly empty).
fn send(client: &AsyncClient, message: Message) {
    if let Err(err) = client.try_publish(
        &message.topic,
        QoS::AtLeastOnce,
        message.retain,
        message.payload,
    ) {
        debug!(topic = message.topic, "dropped MQTT message: {err}");
    }
}

/// Builds the connection settings: a login if given, and a last will so the
/// broker marks us "offline" if we vanish.
fn mqtt_options(config: &HomeAssistant, status_topic: &str) -> MqttOptions {
    // Not "solar-dash": the TeslaMate reader uses that id, and two clients
    // with one id kick each other off a shared broker.
    let mut options = MqttOptions::new(
        "solar-dash-home-assistant",
        &config.mqtt_host,
        config.mqtt_port,
    );
    options.set_keep_alive(KEEP_ALIVE);
    options.set_clean_session(true);
    if let (Some(user), Some(password)) = (&config.mqtt_username, &config.mqtt_password) {
        options.set_credentials(user, password);
    }
    options.set_last_will(LastWill::new(
        status_topic,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    options
}

/// Publishes to Home Assistant until `shutdown` completes. Broker trouble is
/// logged once per outage and retried with a growing pause; it never ends the
/// loop and never touches the bus, which only sees a receiver that is read
/// promptly.
pub async fn run(
    config: HomeAssistant,
    mut events: broadcast::Receiver<HubEvent>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let mut feed = Feed::new(config.clone());
    let status_topic = feed.status_topic();
    let ha_status_topic = feed.ha_status_topic();
    let (client, mut eventloop) = AsyncClient::new(mqtt_options(&config, &status_topic), 128);
    let mut backoff = Backoff::new();
    let mut connected = false;
    let mut failing = false;

    tokio::pin!(shutdown);
    info!(broker = %config.mqtt_host, "Home Assistant MQTT started");
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = eventloop.poll() => match event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    if failing {
                        info!("Home Assistant MQTT back");
                        failing = false;
                    }
                    backoff.reset();
                    connected = true;
                    // The birth message is the only thing we listen to.
                    if let Err(err) = client.try_subscribe(&ha_status_topic, QoS::AtLeastOnce) {
                        warn!("could not subscribe to {ha_status_topic}: {err:#}");
                    }
                    for message in feed.connect_messages() {
                        send(&client, message);
                    }
                }
                Ok(Event::Incoming(Packet::Publish(message))) => {
                    for out in feed.on_ha_status(&message.topic, &message.payload) {
                        send(&client, out);
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    connected = false;
                    if !failing {
                        warn!("Home Assistant MQTT connection failed: {err:#}");
                        failing = true;
                    }
                    // The next `poll` reconnects by itself, after this pause.
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tokio::time::sleep(backoff.pause()) => {}
                    }
                }
            },
            event = events.recv() => match event {
                Ok(event) => {
                    // While down, readings are simply skipped: a stale
                    // reading queued for later would be wrong anyway.
                    if connected {
                        for message in feed.on_event(&event) {
                            send(&client, message);
                        }
                    }
                }
                // Missing some samples only means fewer updates.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    debug!(missed, "Home Assistant feed fell behind");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    // A clean disconnect does not trigger the last will, so say "offline"
    // ourselves, and give the event loop a moment to flush it.
    if connected {
        send(
            &client,
            Message {
                topic: status_topic,
                payload: "offline".into(),
                retain: true,
            },
        );
        let _ = client.try_disconnect();
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while eventloop.poll().await.is_ok() {}
        })
        .await;
    }
    info!("Home Assistant MQTT stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> HomeAssistant {
        toml::from_str("mqtt_host = \"broker\"").unwrap()
    }

    fn sample(ts: i64, grid_w: f64) -> Sample {
        Sample {
            ts,
            pv_w: Some(3200.04),
            load_w: Some(1100.0),
            grid_w: Some(grid_w),
            battery_w: Some(-250.0),
            soc_pct: Some(81.0),
        }
    }

    fn json_of(message: &Message) -> Value {
        serde_json::from_str(&message.payload).unwrap()
    }

    #[test]
    fn discovery_payload_for_one_sensor() {
        let feed = Feed::new(config());
        let message = feed.discovery_message(&LIVE_SENSORS[0]);
        assert_eq!(
            message.topic,
            "homeassistant/sensor/solar_dash/solar_power/config"
        );
        assert!(message.retain);
        assert_eq!(
            json_of(&message),
            json!({
                "name": "Solar power",
                "unique_id": "solar_dash_solar_power",
                "state_topic": "solar-dash/live",
                "value_template": "{{ value_json.solar_w }}",
                "availability_topic": "solar-dash/status",
                "payload_available": "online",
                "payload_not_available": "offline",
                "device_class": "power",
                "state_class": "measurement",
                "unit_of_measurement": "W",
                "suggested_display_precision": 0,
                "device": {
                    "identifiers": ["solar_dash"],
                    "name": "solar-dash",
                    "manufacturer": "solar-dash",
                    "model": "solar-dash",
                    "sw_version": env!("CARGO_PKG_VERSION"),
                },
            })
        );
    }

    #[test]
    fn custom_names_and_prefixes_are_used() {
        let mut cfg = config();
        cfg.device_name = "Home Solar!".into();
        cfg.discovery_prefix = "ha".into();
        cfg.base_topic = "home/solar".into();
        let feed = Feed::new(cfg);
        let message = feed.discovery_message(&LIVE_SENSORS[4]);
        assert_eq!(message.topic, "ha/sensor/home_solar_/battery_soc/config");
        let json = json_of(&message);
        assert_eq!(json["state_topic"], "home/solar/live");
        assert_eq!(json["availability_topic"], "home/solar/status");
        assert_eq!(json["device_class"], "battery");
        assert_eq!(json["device"]["name"], "Home Solar!");
        assert_eq!(feed.ha_status_topic(), "ha/status");
    }

    #[test]
    fn every_sensor_reads_a_field_the_state_message_has() {
        let feed = Feed::new(config());
        let state = json_of(&feed.live_state(&sample(1000, 0.0)));
        let mut ids = std::collections::HashSet::new();
        for sensor in &LIVE_SENSORS {
            assert!(state.get(sensor.field).is_some(), "{}", sensor.field);
            assert!(ids.insert(sensor.object_id));
        }
    }

    #[test]
    fn state_json_from_a_sample_importing() {
        let feed = Feed::new(config());
        let message = feed.live_state(&sample(1000, 450.0));
        assert_eq!(message.topic, "solar-dash/live");
        assert!(!message.retain);
        assert_eq!(
            json_of(&message),
            json!({
                "ts": 1000,
                "solar_w": 3200.0,
                "load_w": 1100.0,
                "grid_w": 450.0,
                "battery_w": -250.0,
                "battery_soc_pct": 81.0,
                "grid_import_w": 450.0,
                "grid_export_w": 0.0,
            })
        );
    }

    #[test]
    fn exporting_gives_negative_grid_and_positive_export() {
        let feed = Feed::new(config());
        let state = json_of(&feed.live_state(&sample(1000, -1800.0)));
        assert_eq!(state["grid_w"], -1800.0);
        assert_eq!(state["grid_import_w"], 0.0);
        assert_eq!(state["grid_export_w"], 1800.0);
    }

    #[test]
    fn a_missing_value_is_null() {
        let feed = Feed::new(config());
        let mut s = sample(1000, 0.0);
        s.grid_w = None;
        s.soc_pct = None;
        let state = json_of(&feed.live_state(&s));
        assert!(state["grid_w"].is_null());
        assert!(state["grid_import_w"].is_null());
        assert!(state["grid_export_w"].is_null());
        assert!(state["battery_soc_pct"].is_null());
    }

    #[test]
    fn state_publishes_are_throttled_by_sample_time() {
        let mut cfg = config();
        cfg.min_interval_secs = 60;
        let mut feed = Feed::new(cfg);
        let sent = |feed: &mut Feed, ts| {
            feed.on_event(&HubEvent::SampleArrived(sample(ts, 0.0)))
                .len()
        };
        assert_eq!(sent(&mut feed, 1000), 1);
        assert_eq!(sent(&mut feed, 1015), 0);
        assert_eq!(sent(&mut feed, 1059), 0);
        assert_eq!(sent(&mut feed, 1060), 1);
        // A clock that stepped back does not freeze the feed.
        assert_eq!(sent(&mut feed, 900), 1);
    }

    #[test]
    fn the_default_interval_passes_every_fifteen_second_sample() {
        let mut feed = Feed::new(config());
        for ts in [1000, 1015, 1030] {
            let out = feed.on_event(&HubEvent::SampleArrived(sample(ts, 0.0)));
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn other_events_publish_nothing() {
        let mut feed = Feed::new(config());
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(feed.on_event(&HubEvent::DayClosed(day)).is_empty());
    }

    #[test]
    fn connecting_sends_online_then_every_config() {
        let mut feed = Feed::new(config());
        let out = feed.connect_messages();
        assert_eq!(out.len(), 1 + LIVE_SENSORS.len());
        assert_eq!(out[0].topic, "solar-dash/status");
        assert_eq!(out[0].payload, "online");
        assert!(out[0].retain);
        assert!(
            out[1..]
                .iter()
                .all(|m| m.retain && m.topic.ends_with("/config"))
        );
    }

    #[test]
    fn discovery_is_resent_only_when_home_assistant_says_online() {
        let mut feed = Feed::new(config());
        assert!(
            !feed
                .on_ha_status("homeassistant/status", b"online")
                .is_empty()
        );
        assert!(
            feed.on_ha_status("homeassistant/status", b"offline")
                .is_empty()
        );
        assert!(feed.on_ha_status("other/status", b"online").is_empty());
    }

    #[test]
    fn reconnecting_restarts_the_throttle() {
        let mut cfg = config();
        cfg.min_interval_secs = 600;
        let mut feed = Feed::new(cfg);
        let event = HubEvent::SampleArrived(sample(1000, 0.0));
        assert_eq!(feed.on_event(&event).len(), 1);
        assert!(feed.on_event(&event).is_empty());
        feed.connect_messages();
        assert_eq!(feed.on_event(&event).len(), 1);
    }

    #[test]
    fn the_last_will_marks_us_offline() {
        let options = mqtt_options(&config(), "solar-dash/status");
        let will = options.last_will().unwrap();
        assert_eq!(will.topic, "solar-dash/status");
        assert_eq!(&will.message[..], b"offline");
        assert!(will.retain);
    }
}
