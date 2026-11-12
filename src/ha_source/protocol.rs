//! The Home Assistant WebSocket API as plain data: what we send and how we
//! read what comes back. Nothing in here touches the network, so the whole
//! conversation is tested by feeding it text.
//!
//! The conversation, in order:
//!
//! 1. Home Assistant says `auth_required`; we send `auth` with the token.
//! 2. It answers `auth_ok` (go on) or `auth_invalid` (the token is wrong).
//! 3. We send `subscribe_trigger` (a state trigger on our entity ids, so
//!    Home Assistant only sends the ones we asked for) and `get_states`
//!    (every entity's current state; we keep only ours). Subscribing first
//!    means a change between the two is not missed.
//! 4. Then Home Assistant sends an `event` for each change.
//!
//! Everything here is read-only: the only commands are `auth`,
//! `subscribe_trigger` and `get_states`. There is no `call_service`.

use std::collections::HashSet;

use anyhow::{Result, bail};
use chrono::DateTime;
use serde_json::{Value, json};

/// Message id of our subscription. Home Assistant wants ids that only go up.
const SUBSCRIBE_ID: u64 = 1;
/// Message id of the one `get_states` request.
const STATES_ID: u64 = 2;

/// One entity's state at a moment, as Home Assistant reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct StateUpdate {
    pub entity_id: String,
    /// The state text, such as "412.5", "on", "unknown" or "unavailable".
    pub state: String,
    /// The `unit_of_measurement` attribute, if the entity has one.
    pub unit: Option<String>,
    /// When the state last changed, unix seconds UTC.
    pub ts: i64,
}

impl StateUpdate {
    /// The state as a number, or `None` for text, `unknown`, `unavailable`
    /// and anything that is not a finite number (so a NaN never reaches the
    /// database).
    pub fn numeric(&self) -> Option<f64> {
        let value: f64 = self.state.trim().parse().ok()?;
        value.is_finite().then_some(value)
    }
}

/// What the session wants done after a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this text to Home Assistant.
    Send(String),
    /// States of our entities (from `get_states` or an event).
    Updates(Vec<StateUpdate>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for `auth_required`.
    Start,
    /// Token sent, waiting for the verdict.
    Authenticating,
    /// Logged in; subscribed.
    Running,
}

/// One WebSocket conversation (a new one per connection).
pub struct Session {
    token: String,
    /// The entities we read, in config order.
    entity_ids: Vec<String>,
    wanted: HashSet<String>,
    phase: Phase,
}

impl Session {
    pub fn new(token: &str, entity_ids: Vec<String>) -> Self {
        let wanted = entity_ids.iter().cloned().collect();
        Self {
            token: token.to_string(),
            entity_ids,
            wanted,
            phase: Phase::Start,
        }
    }

    /// True once Home Assistant has accepted the token.
    pub fn is_authenticated(&self) -> bool {
        self.phase == Phase::Running
    }

    /// Handles one text message. `now` (unix seconds) stands in when a state
    /// has no usable `last_updated`. An `Err` ends the connection: a refused
    /// token, a refused command or text that is not the protocol.
    pub fn on_text(&mut self, text: &str, now: i64) -> Result<Vec<Step>> {
        let message: Value = serde_json::from_str(text)
            .map_err(|_| anyhow::anyhow!("Home Assistant sent text that is not JSON"))?;
        let kind = message["type"].as_str().unwrap_or_default();
        match (self.phase, kind) {
            (Phase::Start, "auth_required") => {
                self.phase = Phase::Authenticating;
                Ok(vec![Step::Send(
                    json!({"type": "auth", "access_token": self.token}).to_string(),
                )])
            }
            (Phase::Authenticating, "auth_ok") => {
                self.phase = Phase::Running;
                Ok(vec![
                    Step::Send(subscribe_request(&self.entity_ids)),
                    Step::Send(json!({"id": STATES_ID, "type": "get_states"}).to_string()),
                ])
            }
            (Phase::Authenticating, "auth_invalid") => {
                let why = message["message"].as_str().unwrap_or("no reason given");
                bail!("Home Assistant refused the access token: {why}")
            }
            (Phase::Running, "result") => self.on_result(&message, now),
            (Phase::Running, "event") if message["id"] == SUBSCRIBE_ID => {
                let state = message
                    .pointer("/event/variables/trigger/to_state")
                    // The shape `subscribe_events` would give; harmless to accept.
                    .or_else(|| message.pointer("/event/data/new_state"));
                Ok(self.keep_ours(state.into_iter(), now))
            }
            // Anything else (ping replies, `auth_required` again, ...) is
            // not for us.
            _ => Ok(Vec::new()),
        }
    }

    fn on_result(&mut self, message: &Value, now: i64) -> Result<Vec<Step>> {
        let id = message["id"].as_u64();
        if message["success"] != true {
            let why = message
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            let what = match id {
                Some(SUBSCRIBE_ID) => "subscribe to state changes",
                Some(STATES_ID) => "list the states",
                _ => "answer a request",
            };
            bail!("Home Assistant would not {what}: {why}");
        }
        if id != Some(STATES_ID) {
            return Ok(Vec::new());
        }
        let states = message["result"].as_array().map(Vec::as_slice);
        Ok(self.keep_ours(states.unwrap_or_default().iter(), now))
    }

    /// The updates among `states` that are for an entity we asked about.
    fn keep_ours<'a>(&self, states: impl Iterator<Item = &'a Value>, now: i64) -> Vec<Step> {
        let updates: Vec<StateUpdate> = states
            .filter_map(|state| parse_state(state, now))
            .filter(|update| self.wanted.contains(&update.entity_id))
            .collect();
        if updates.is_empty() {
            Vec::new()
        } else {
            vec![Step::Updates(updates)]
        }
    }
}

/// The `subscribe_trigger` request: fire on a state change of any of the
/// entities. Home Assistant does the filtering, so a busy install does not
/// send us every sensor it has.
fn subscribe_request(entity_ids: &[String]) -> String {
    json!({
        "id": SUBSCRIBE_ID,
        "type": "subscribe_trigger",
        "trigger": {"platform": "state", "entity_id": entity_ids},
    })
    .to_string()
}

/// Reads one state object (`{"entity_id", "state", "attributes",
/// "last_updated"}`); `None` if it is not one (a removed entity has a null
/// `to_state`).
fn parse_state(state: &Value, now: i64) -> Option<StateUpdate> {
    let entity_id = state["entity_id"].as_str()?.to_string();
    let text = state["state"].as_str()?.to_string();
    let unit = state
        .pointer("/attributes/unit_of_measurement")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ts = state["last_updated"]
        .as_str()
        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        .map_or(now, |time| time.timestamp());
    Some(StateUpdate {
        entity_id,
        state: text,
        unit,
        ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn session() -> Session {
        Session::new(
            "tok",
            vec!["sensor.solar_power".into(), "sensor.temp".into()],
        )
    }

    fn sent(steps: &[Step]) -> Vec<Value> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Send(text) => Some(serde_json::from_str(text).unwrap()),
                _ => None,
            })
            .collect()
    }

    fn logged_in() -> Session {
        let mut s = session();
        s.on_text(r#"{"type":"auth_required","ha_version":"2026.1.0"}"#, NOW)
            .unwrap();
        s.on_text(r#"{"type":"auth_ok","ha_version":"2026.1.0"}"#, NOW)
            .unwrap();
        s
    }

    #[test]
    fn auth_flow_sends_token_then_subscribes_and_asks_for_states() {
        let mut s = session();
        assert!(!s.is_authenticated());
        let steps = s.on_text(r#"{"type":"auth_required"}"#, NOW).unwrap();
        assert_eq!(
            sent(&steps),
            [json!({"type": "auth", "access_token": "tok"})]
        );
        let steps = s.on_text(r#"{"type":"auth_ok"}"#, NOW).unwrap();
        assert!(s.is_authenticated());
        let out = sent(&steps);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "subscribe_trigger");
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["trigger"]["platform"], "state");
        assert_eq!(
            out[0]["trigger"]["entity_id"],
            json!(["sensor.solar_power", "sensor.temp"])
        );
        assert_eq!(out[1], json!({"id": 2, "type": "get_states"}));
    }

    #[test]
    fn only_read_commands_are_ever_sent() {
        let mut s = session();
        let mut all = sent(&s.on_text(r#"{"type":"auth_required"}"#, NOW).unwrap());
        all.extend(sent(&s.on_text(r#"{"type":"auth_ok"}"#, NOW).unwrap()));
        for message in all {
            let kind = message["type"].as_str().unwrap();
            assert!(
                ["auth", "subscribe_trigger", "get_states"].contains(&kind),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_refused_token_is_an_error_that_does_not_repeat_the_token() {
        let mut s = session();
        s.on_text(r#"{"type":"auth_required"}"#, NOW).unwrap();
        let err = s
            .on_text(
                r#"{"type":"auth_invalid","message":"Invalid access token or password"}"#,
                NOW,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid access token"), "{err}");
        assert!(!err.contains("tok\""));
    }

    #[test]
    fn get_states_keeps_only_our_entities() {
        let mut s = logged_in();
        let steps = s
            .on_text(
                r#"{"id":2,"type":"result","success":true,"result":[
                  {"entity_id":"sensor.solar_power","state":"412.5",
                   "attributes":{"unit_of_measurement":"W"},
                   "last_updated":"2026-11-12T01:02:03.456789+00:00"},
                  {"entity_id":"light.kitchen","state":"on","attributes":{}},
                  {"entity_id":"sensor.temp","state":"unavailable","attributes":{}}]}"#,
                NOW,
            )
            .unwrap();
        let [Step::Updates(updates)] = steps.as_slice() else {
            panic!("{steps:?}");
        };
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].entity_id, "sensor.solar_power");
        assert_eq!(updates[0].unit.as_deref(), Some("W"));
        assert_eq!(updates[0].numeric(), Some(412.5));
        assert_eq!(
            updates[0].ts,
            DateTime::parse_from_rfc3339("2026-11-12T01:02:03Z")
                .unwrap()
                .timestamp()
        );
        // No last_updated: the caller's clock is used. Not a number.
        assert_eq!(updates[1].ts, NOW);
        assert_eq!(updates[1].numeric(), None);
    }

    #[test]
    fn state_changed_event_gives_an_update() {
        let mut s = logged_in();
        let steps = s
            .on_text(
                r#"{"id":1,"type":"event","event":{"variables":{"trigger":{
                  "platform":"state","entity_id":"sensor.solar_power",
                  "from_state":{"entity_id":"sensor.solar_power","state":"1"},
                  "to_state":{"entity_id":"sensor.solar_power","state":"2500",
                    "attributes":{"unit_of_measurement":"W"},
                    "last_updated":"2026-11-12T01:02:03+00:00"}}}}}"#,
                NOW,
            )
            .unwrap();
        let [Step::Updates(updates)] = steps.as_slice() else {
            panic!("{steps:?}");
        };
        assert_eq!(updates[0].numeric(), Some(2500.0));
    }

    #[test]
    fn plain_state_changed_events_also_parse() {
        let mut s = logged_in();
        let steps = s
            .on_text(
                r#"{"id":1,"type":"event","event":{"event_type":"state_changed","data":{
                  "entity_id":"sensor.temp","new_state":{"entity_id":"sensor.temp","state":"21.5"}}}}"#,
                NOW,
            )
            .unwrap();
        assert!(matches!(steps.as_slice(), [Step::Updates(u)] if u[0].numeric() == Some(21.5)));
    }

    #[test]
    fn removed_entities_other_ids_and_other_entities_are_ignored() {
        let mut s = logged_in();
        // to_state null: the entity was removed.
        let removed =
            r#"{"id":1,"type":"event","event":{"variables":{"trigger":{"to_state":null}}}}"#;
        assert!(s.on_text(removed, NOW).unwrap().is_empty());
        // An event for a subscription we did not make.
        let other = r#"{"id":9,"type":"event","event":{"variables":{"trigger":{
            "to_state":{"entity_id":"sensor.temp","state":"1"}}}}}"#;
        assert!(s.on_text(other, NOW).unwrap().is_empty());
        // An entity we did not ask for.
        let stranger = r#"{"id":1,"type":"event","event":{"variables":{"trigger":{
            "to_state":{"entity_id":"light.x","state":"on"}}}}}"#;
        assert!(s.on_text(stranger, NOW).unwrap().is_empty());
        // The subscription's own acknowledgement.
        assert!(
            s.on_text(
                r#"{"id":1,"type":"result","success":true,"result":null}"#,
                NOW
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn a_refused_command_is_an_error() {
        let mut s = logged_in();
        let err = s
            .on_text(
                r#"{"id":1,"type":"result","success":false,"error":{"code":"unauthorized","message":"Unauthorized"}}"#,
                NOW,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("subscribe") && err.contains("Unauthorized"),
            "{err}"
        );
    }

    #[test]
    fn messages_out_of_order_or_not_json_are_handled() {
        let mut s = session();
        // An event before logging in is not for us, and not an error.
        assert!(
            s.on_text(r#"{"id":1,"type":"event"}"#, NOW)
                .unwrap()
                .is_empty()
        );
        assert!(s.on_text("not json", NOW).is_err());
    }

    #[test]
    fn numeric_states_only() {
        let update = |state: &str| StateUpdate {
            entity_id: "sensor.x".into(),
            state: state.into(),
            unit: None,
            ts: 0,
        };
        assert_eq!(update("12").numeric(), Some(12.0));
        assert_eq!(update(" -3.5 ").numeric(), Some(-3.5));
        assert_eq!(update("1e3").numeric(), Some(1000.0));
        for text in ["unknown", "unavailable", "on", "", "NaN", "inf", "-inf"] {
            assert_eq!(update(text).numeric(), None, "{text:?}");
        }
    }
}
