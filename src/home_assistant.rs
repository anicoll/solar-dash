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
//! Two state topics feed the sensors: `<base_topic>/live` (one message per
//! inverter sample) and `<base_topic>/today` (today's energy, cost and the
//! Amber prices, about once a minute and again when a day closes).
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
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::config::HomeAssistant;
use crate::hub::HubEvent;
use crate::modbus::Sample;
use crate::teslamate::Backoff;
use crate::web::{DayEnergy, DayMoney};

const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// How often today's energy, cost and prices are sent.
const TODAY_EVERY: Duration = Duration::from_secs(60);

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
    /// `<base_topic>/today`: today's totals, prices and cost, retained so a
    /// restarted Home Assistant has values at once.
    Today,
}

impl StateTopic {
    fn suffix(self) -> &'static str {
        match self {
            StateTopic::Live => "live",
            StateTopic::Today => "today",
        }
    }

    /// Whether the broker keeps the last message. A live reading is only true
    /// for now, so it is not kept; today's totals are, and every message says
    /// which `date` it is for.
    fn retained(self) -> bool {
        matches!(self, StateTopic::Today)
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
    /// `None` for text and for money (Home Assistant only allows `total` for
    /// money, which suits a figure that can also fall).
    pub state_class: Option<&'static str>,
    /// `None` for a text sensor.
    pub unit: Option<&'static str>,
    pub precision: Option<u8>,
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
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
    Sensor {
        object_id: "home_load",
        name: "Home load",
        topic: StateTopic::Live,
        field: "load_w",
        device_class: Some("power"),
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
    Sensor {
        object_id: "grid_power",
        name: "Grid power",
        topic: StateTopic::Live,
        field: "grid_w",
        device_class: Some("power"),
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
    Sensor {
        object_id: "battery_power",
        name: "Battery power",
        topic: StateTopic::Live,
        field: "battery_w",
        device_class: Some("power"),
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
    Sensor {
        object_id: "battery_soc",
        name: "Battery state of charge",
        topic: StateTopic::Live,
        field: "battery_soc_pct",
        device_class: Some("battery"),
        state_class: Some("measurement"),
        unit: Some("%"),
        precision: Some(0),
    },
    Sensor {
        object_id: "grid_import",
        name: "Grid import",
        topic: StateTopic::Live,
        field: "grid_import_w",
        device_class: Some("power"),
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
    Sensor {
        object_id: "grid_export",
        name: "Grid export",
        topic: StateTopic::Live,
        field: "grid_export_w",
        device_class: Some("power"),
        state_class: Some("measurement"),
        unit: Some("W"),
        precision: Some(0),
    },
];

/// Today's energy since local midnight, for Home Assistant's Energy
/// dashboard. `total_increasing` tells it the value only grows, and that a
/// fall (the reset at local midnight) starts a new day, not a negative
/// amount. The figures are the Today page's own day totals.
pub const ENERGY_SENSORS: [Sensor; 6] = [
    Sensor {
        object_id: "today_grid_import",
        name: "Grid import today",
        topic: StateTopic::Today,
        field: "import_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_grid_export",
        name: "Grid export today",
        topic: StateTopic::Today,
        field: "export_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_solar",
        name: "Solar today",
        topic: StateTopic::Today,
        field: "solar_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_battery_charge",
        name: "Battery charge today",
        topic: StateTopic::Today,
        field: "battery_charge_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_battery_discharge",
        name: "Battery discharge today",
        topic: StateTopic::Today,
        field: "battery_discharge_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_home_use",
        name: "Home use today",
        topic: StateTopic::Today,
        field: "home_kwh",
        device_class: Some("energy"),
        state_class: Some("total_increasing"),
        unit: Some("kWh"),
        precision: Some(2),
    },
];

/// Today's cost so far, in Australian dollars, from the same cost engine as
/// the Today page. Only offered when the plan is set up (`[tariff]` or the
/// Admin page), see [`Feed::new`].
pub const COST_SENSORS: [Sensor; 2] = [
    Sensor {
        object_id: "today_power_cost",
        name: "Power cost today",
        topic: StateTopic::Today,
        field: "cost_power_aud",
        device_class: Some("monetary"),
        state_class: None,
        unit: Some("AUD"),
        precision: Some(2),
    },
    Sensor {
        object_id: "today_net_cost",
        name: "Net cost today",
        topic: StateTopic::Today,
        field: "cost_net_aud",
        device_class: Some("monetary"),
        state_class: None,
        unit: Some("AUD"),
        precision: Some(2),
    },
];

/// Amber's current prices. Only offered when `[amber]` is configured.
pub const AMBER_SENSORS: [Sensor; 3] = [
    Sensor {
        object_id: "amber_general_price",
        name: "Amber general price",
        topic: StateTopic::Today,
        field: "amber_general_cents",
        device_class: None,
        state_class: Some("measurement"),
        unit: Some("c/kWh"),
        precision: Some(1),
    },
    Sensor {
        object_id: "amber_feed_in_price",
        name: "Amber feed-in price",
        topic: StateTopic::Today,
        field: "amber_feed_in_cents",
        device_class: None,
        state_class: Some("measurement"),
        unit: Some("c/kWh"),
        precision: Some(1),
    },
    Sensor {
        object_id: "amber_descriptor",
        name: "Amber price level",
        topic: StateTopic::Today,
        field: "amber_descriptor",
        device_class: None,
        state_class: None,
        unit: None,
        precision: None,
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

/// Which optional sensors this install has. Decided once at startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct Offer {
    /// `[amber]` is configured: add the Amber price sensors.
    pub amber: bool,
    /// A tariff is set up: add the cost sensors. (A plan added later needs a
    /// restart to show up in Home Assistant.)
    pub costs: bool,
}

/// What `<base_topic>/today` is made from. Built by the spoke from the same
/// functions the Today page uses.
#[derive(Debug, Clone, PartialEq)]
pub struct TodaySnapshot {
    /// The local day all the figures are for. It changes at local midnight,
    /// which is what resets the energy sensors.
    pub date: NaiveDate,
    pub energy: DayEnergy,
    pub money: Option<DayMoney>,
    pub amber: Option<AmberNow>,
}

/// Amber's prices right now, in c/kWh.
#[derive(Debug, Clone, PartialEq)]
pub struct AmberNow {
    /// What buying costs.
    pub general_cents: Option<f64>,
    /// What selling pays, **as stored**: Amber sends a negative number when
    /// you are paid. [`Feed::today_state`] flips it for Home Assistant.
    pub feed_in_raw_cents: Option<f64>,
    /// Amber's label for the buy price: extremelyLow, veryLow, low, neutral,
    /// high or spike.
    pub descriptor: Option<String>,
}

/// The local date at unix time `now` in `tz`. This is what "today" means for
/// the daily sensors, so they reset at local midnight, not at UTC midnight.
pub fn local_date(now: i64, tz: Tz) -> NaiveDate {
    Utc.timestamp_opt(now, 0)
        .single()
        .map(|t| t.with_timezone(&tz).date_naive())
        .unwrap_or_default()
}

/// Decides when to publish and what. Holds the throttle clock.
pub struct Feed {
    config: HomeAssistant,
    offer: Offer,
    node_id: String,
    /// Sample time (unix seconds) of the last state message sent.
    last_state_ts: Option<i64>,
    /// Set when `<base_topic>/today` should be sent again soon; see
    /// [`Feed::take_today_request`].
    today_wanted: bool,
}

impl Feed {
    pub fn new(config: HomeAssistant, offer: Offer) -> Self {
        let node_id = node_id(&config.device_name);
        Self {
            config,
            offer,
            node_id,
            last_state_ts: None,
            today_wanted: false,
        }
    }

    /// Every sensor this install offers.
    pub fn sensors(&self) -> Vec<Sensor> {
        let mut all: Vec<Sensor> = LIVE_SENSORS.to_vec();
        all.extend(ENERGY_SENSORS);
        if self.offer.costs {
            all.extend(COST_SENSORS);
        }
        if self.offer.amber {
            all.extend(AMBER_SENSORS);
        }
        all
    }

    /// Asks for today's message to be rebuilt and sent (the once-a-minute
    /// timer, a day closing, new prices, a reconnect).
    pub fn request_today(&mut self) {
        self.today_wanted = true;
    }

    /// Whether today's message is wanted, and clears the request.
    pub fn take_today_request(&mut self) -> bool {
        std::mem::take(&mut self.today_wanted)
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
            "device": {
                "identifiers": [self.node_id],
                "name": self.config.device_name,
                "manufacturer": "solar-dash",
                "model": "solar-dash",
                "sw_version": env!("CARGO_PKG_VERSION"),
            },
        });
        // Text sensors have no unit, class or precision; money has no state class.
        if let Some(class) = sensor.device_class {
            payload["device_class"] = json!(class);
        }
        if let Some(class) = sensor.state_class {
            payload["state_class"] = json!(class);
        }
        if let Some(unit) = sensor.unit {
            payload["unit_of_measurement"] = json!(unit);
        }
        if let Some(precision) = sensor.precision {
            payload["suggested_display_precision"] = json!(precision);
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
        self.request_today();
        let mut out = vec![Message {
            topic: self.status_topic(),
            payload: "online".into(),
            retain: true,
        }];
        out.extend(self.sensors().iter().map(|s| self.discovery_message(s)));
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

    /// The JSON state message for `<base_topic>/today`.
    ///
    /// Energy is in kWh since local midnight of `date`; it is `null` when the
    /// day has no data yet, never an invented 0. Money is in AUD (dollars).
    /// `amber_feed_in_cents` is **what you earn**: Amber's raw feed-in price is
    /// negative when you are paid, so it is flipped here, like the UI does
    /// (a positive number means you are paid, a negative one that you pay to
    /// export).
    pub fn today_state(&self, today: &TodaySnapshot) -> Message {
        let e = &today.energy;
        let kwh = |v: Option<f64>| v.map(|v| (v * 1000.0).round() / 1000.0);
        let mut state = json!({
            "date": today.date.to_string(),
            "import_kwh": kwh(e.import_kwh),
            "export_kwh": kwh(e.export_kwh),
            "solar_kwh": kwh(e.pv_kwh),
            "battery_charge_kwh": kwh(e.battery_charge_kwh),
            "battery_discharge_kwh": kwh(e.battery_discharge_kwh),
            "home_kwh": kwh(e.load_kwh),
        });
        if let Some(money) = &today.money {
            state["cost_power_aud"] = json!(money.power);
            state["cost_net_aud"] = json!(money.net);
        }
        if let Some(amber) = &today.amber {
            state["amber_general_cents"] = json!(amber.general_cents.map(round1));
            // `0.0 - x` instead of `-x` so that a raw 0 gives 0, not -0.
            state["amber_feed_in_cents"] = json!(amber.feed_in_raw_cents.map(|c| round1(0.0 - c)));
            state["amber_descriptor"] = json!(amber.descriptor);
        }
        Message {
            topic: self.state_topic(StateTopic::Today),
            payload: state.to_string(),
            retain: StateTopic::Today.retained(),
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
            // A new day: the daily sensors start again from 0. The message is
            // built from the clock's date, so it carries the new day.
            HubEvent::DayClosed(_) => {
                self.request_today();
                Vec::new()
            }
            // New prices change the Amber sensors.
            HubEvent::PricesStored { .. } => {
                self.request_today();
                Vec::new()
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
///
/// `today` builds the figures for `<base_topic>/today`; it is called about
/// once a minute and when a day closes, never from the bus handling.
pub async fn run<F, Fut>(
    config: HomeAssistant,
    offer: Offer,
    mut events: broadcast::Receiver<HubEvent>,
    today: F,
    shutdown: impl Future<Output = ()>,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<TodaySnapshot>>,
{
    let mut feed = Feed::new(config.clone(), offer);
    let mut minute = tokio::time::interval(TODAY_EVERY);
    minute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
            _ = minute.tick() => feed.request_today(),
        }
        // Outside the `select!` so the database read below is not cut off
        // by another branch winning. It takes milliseconds, and the events
        // that arrive meanwhile wait in the bus's buffer.
        if connected && feed.take_today_request() {
            match today().await {
                Ok(snapshot) => send(&client, feed.today_state(&snapshot)),
                Err(err) => warn!("could not work out today's figures: {err:#}"),
            }
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
        let feed = Feed::new(config(), Offer::default());
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
        let feed = Feed::new(cfg, Offer::default());
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
    fn every_live_sensor_reads_a_field_the_state_message_has() {
        let feed = Feed::new(config(), Offer::default());
        let state = json_of(&feed.live_state(&sample(1000, 0.0)));
        let mut ids = std::collections::HashSet::new();
        for sensor in &LIVE_SENSORS {
            assert!(state.get(sensor.field).is_some(), "{}", sensor.field);
            assert!(ids.insert(sensor.object_id));
        }
    }

    #[test]
    fn state_json_from_a_sample_importing() {
        let feed = Feed::new(config(), Offer::default());
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
        let feed = Feed::new(config(), Offer::default());
        let state = json_of(&feed.live_state(&sample(1000, -1800.0)));
        assert_eq!(state["grid_w"], -1800.0);
        assert_eq!(state["grid_import_w"], 0.0);
        assert_eq!(state["grid_export_w"], 1800.0);
    }

    #[test]
    fn a_missing_value_is_null() {
        let feed = Feed::new(config(), Offer::default());
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
        let mut feed = Feed::new(cfg, Offer::default());
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
        let mut feed = Feed::new(config(), Offer::default());
        for ts in [1000, 1015, 1030] {
            let out = feed.on_event(&HubEvent::SampleArrived(sample(ts, 0.0)));
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn other_events_publish_nothing() {
        let mut feed = Feed::new(config(), Offer::default());
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        assert!(feed.on_event(&HubEvent::DayClosed(day)).is_empty());
    }

    #[test]
    fn connecting_sends_online_then_every_config() {
        let mut feed = Feed::new(config(), Offer::default());
        let out = feed.connect_messages();
        assert_eq!(out.len(), 1 + LIVE_SENSORS.len() + ENERGY_SENSORS.len());
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
        let mut feed = Feed::new(config(), Offer::default());
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
        let mut feed = Feed::new(cfg, Offer::default());
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

    fn snapshot(date: NaiveDate) -> TodaySnapshot {
        TodaySnapshot {
            date,
            energy: DayEnergy {
                pv_kwh: Some(18.4567),
                load_kwh: Some(12.0),
                import_kwh: Some(3.2),
                export_kwh: Some(9.9994),
                battery_charge_kwh: Some(4.5),
                battery_discharge_kwh: Some(2.25),
            },
            money: Some(DayMoney {
                power: -0.42,
                net: 0.58,
            }),
            amber: Some(AmberNow {
                general_cents: Some(27.04),
                feed_in_raw_cents: Some(-8.46),
                descriptor: Some("low".into()),
            }),
        }
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    #[test]
    fn energy_discovery_is_for_the_energy_dashboard() {
        let feed = Feed::new(config(), Offer::default());
        let sensor = ENERGY_SENSORS
            .iter()
            .find(|s| s.object_id == "today_solar")
            .unwrap();
        let message = feed.discovery_message(sensor);
        assert_eq!(
            message.topic,
            "homeassistant/sensor/solar_dash/today_solar/config"
        );
        assert!(message.retain);
        let json = json_of(&message);
        assert_eq!(json["name"], "Solar today");
        assert_eq!(json["state_topic"], "solar-dash/today");
        assert_eq!(json["value_template"], "{{ value_json.solar_kwh }}");
        assert_eq!(json["device_class"], "energy");
        assert_eq!(json["state_class"], "total_increasing");
        assert_eq!(json["unit_of_measurement"], "kWh");
        assert_eq!(json["suggested_display_precision"], 2);
        assert_eq!(json["device"]["identifiers"], json!(["solar_dash"]));
        assert_eq!(ENERGY_SENSORS.len(), 6);
    }

    #[test]
    fn today_state_carries_the_days_figures() {
        let feed = Feed::new(
            config(),
            Offer {
                amber: true,
                costs: true,
            },
        );
        let message = feed.today_state(&snapshot(day(11)));
        assert_eq!(message.topic, "solar-dash/today");
        // Retained, because every message says which day it is for.
        assert!(message.retain);
        assert_eq!(
            json_of(&message),
            json!({
                "date": "2026-11-11",
                "import_kwh": 3.2,
                "export_kwh": 9.999,
                "solar_kwh": 18.457,
                "battery_charge_kwh": 4.5,
                "battery_discharge_kwh": 2.25,
                "home_kwh": 12.0,
                "cost_power_aud": -0.42,
                "cost_net_aud": 0.58,
                "amber_general_cents": 27.0,
                "amber_feed_in_cents": 8.5,
                "amber_descriptor": "low",
            })
        );
    }

    #[test]
    fn a_negative_raw_feed_in_price_is_shown_as_earned() {
        let feed = Feed::new(
            config(),
            Offer {
                amber: true,
                costs: false,
            },
        );
        let with = |raw: f64| {
            let mut s = snapshot(day(11));
            s.amber.as_mut().unwrap().feed_in_raw_cents = Some(raw);
            json_of(&feed.today_state(&s))["amber_feed_in_cents"].clone()
        };
        assert_eq!(with(-8.46), json!(8.5)); // paid 8.5 c to export
        assert_eq!(with(3.0), json!(-3.0)); // paying 3 c to export
        assert_eq!(with(0.0), json!(0.0));
    }

    #[test]
    fn amber_and_cost_sensors_are_only_offered_when_configured() {
        let ids = |offer: Offer| -> Vec<&'static str> {
            Feed::new(config(), offer)
                .sensors()
                .iter()
                .map(|s| s.object_id)
                .collect()
        };
        let plain = ids(Offer::default());
        assert!(plain.contains(&"today_solar"));
        assert!(!plain.iter().any(|id| id.starts_with("amber_")));
        assert!(!plain.contains(&"today_power_cost"));

        let all = ids(Offer {
            amber: true,
            costs: true,
        });
        for id in [
            "amber_general_price",
            "amber_feed_in_price",
            "amber_descriptor",
            "today_power_cost",
            "today_net_cost",
        ] {
            assert!(all.contains(&id), "{id}");
        }
        // No two sensors share an id.
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
        // And the discovery messages follow the same list.
        let mut feed = Feed::new(
            config(),
            Offer {
                amber: true,
                costs: true,
            },
        );
        assert_eq!(feed.connect_messages().len(), 1 + all.len());
    }

    #[test]
    fn price_money_and_text_sensor_payloads() {
        let feed = Feed::new(
            config(),
            Offer {
                amber: true,
                costs: true,
            },
        );
        let find = |id: &str| {
            feed.sensors()
                .into_iter()
                .find(|s| s.object_id == id)
                .unwrap()
        };
        let money = json_of(&feed.discovery_message(&find("today_power_cost")));
        assert_eq!(money["device_class"], "monetary");
        assert_eq!(money["unit_of_measurement"], "AUD");
        assert!(money.get("state_class").is_none());

        let price = json_of(&feed.discovery_message(&find("amber_feed_in_price")));
        assert_eq!(price["unit_of_measurement"], "c/kWh");
        assert_eq!(price["state_class"], "measurement");
        assert!(price.get("device_class").is_none());

        let text = json_of(&feed.discovery_message(&find("amber_descriptor")));
        assert_eq!(text["value_template"], "{{ value_json.amber_descriptor }}");
        for key in [
            "unit_of_measurement",
            "state_class",
            "device_class",
            "suggested_display_precision",
        ] {
            assert!(text.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn today_state_without_money_or_amber_leaves_those_keys_out() {
        let feed = Feed::new(config(), Offer::default());
        let mut s = snapshot(day(11));
        s.money = None;
        s.amber = None;
        let json = json_of(&feed.today_state(&s));
        assert!(json.get("cost_net_aud").is_none());
        assert!(json.get("amber_general_cents").is_none());
        assert_eq!(json["solar_kwh"], 18.457);
    }

    /// A day with no data is `null`, not 0, and nothing of yesterday is kept.
    #[test]
    fn a_new_day_starts_afresh_at_local_midnight() {
        let mut feed = Feed::new(config(), Offer::default());
        let yesterday = json_of(&feed.today_state(&snapshot(day(10))));
        assert_eq!(yesterday["date"], "2026-11-10");

        // Closing the day asks for a fresh message at once.
        assert!(!feed.take_today_request());
        let out = feed.on_event(&HubEvent::DayClosed(day(10)));
        assert!(out.is_empty());
        assert!(feed.take_today_request());
        assert!(!feed.take_today_request());

        // The new day has no readings yet.
        let fresh = TodaySnapshot {
            date: day(11),
            energy: DayEnergy::default(),
            money: None,
            amber: None,
        };
        let today = json_of(&feed.today_state(&fresh));
        assert_eq!(today["date"], "2026-11-11");
        for key in [
            "import_kwh",
            "export_kwh",
            "solar_kwh",
            "battery_charge_kwh",
            "battery_discharge_kwh",
            "home_kwh",
        ] {
            assert!(today[key].is_null(), "{key}");
        }
    }

    #[test]
    fn today_means_the_local_day_not_the_utc_day() {
        let adelaide = chrono_tz::Australia::Adelaide;
        // In November Adelaide is on daylight time (+10:30), so 13:29:59 UTC
        // is 23:59:59 local on the 10th.
        let before = Utc
            .with_ymd_and_hms(2026, 11, 10, 13, 29, 59)
            .unwrap()
            .timestamp();
        assert_eq!(local_date(before, adelaide), day(10));
        assert_eq!(local_date(before + 1, adelaide), day(11));
        // At the same instants UTC is still on the 10th.
        assert_eq!(local_date(before + 1, chrono_tz::UTC), day(10));
    }

    #[test]
    fn day_close_prices_and_connect_all_ask_for_today() {
        let mut feed = Feed::new(config(), Offer::default());
        feed.on_event(&HubEvent::PricesStored {
            source: "amber:x".into(),
        });
        assert!(feed.take_today_request());
        feed.connect_messages();
        assert!(feed.take_today_request());
        feed.on_ha_status("homeassistant/status", b"online");
        assert!(feed.take_today_request());
    }
}
