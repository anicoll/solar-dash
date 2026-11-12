use super::*;
use crate::config::Config;

fn entity(id: &str, metric: Option<Metric>, unit: Option<&str>) -> HaEntity {
    HaEntity {
        entity_id: id.into(),
        metric,
        name: None,
        unit: unit.map(str::to_string),
        invert: false,
    }
}

fn update(id: &str, state: &str, unit: Option<&str>, ts: i64) -> StateUpdate {
    StateUpdate {
        entity_id: id.into(),
        state: state.into(),
        unit: unit.map(str::to_string),
        ts,
    }
}

fn ingest(entities: &[HaEntity]) -> Ingest {
    Ingest::new(entities, HaView::new(entities))
}

#[test]
fn units_are_converted_to_the_metrics_unit() {
    assert_eq!(to_metric_unit(Metric::PvW, Some("W"), 5.0), Some(5.0));
    assert_eq!(to_metric_unit(Metric::PvW, Some("kW"), 1.5), Some(1500.0));
    assert_eq!(
        to_metric_unit(Metric::LoadDayWh, Some("kWh"), 2.0),
        Some(2000.0)
    );
    assert_eq!(to_metric_unit(Metric::SocPct, Some("%"), 80.0), Some(80.0));
    // A unit that does not fit, or none: not guessed.
    assert_eq!(to_metric_unit(Metric::PvW, Some("kWh"), 1.0), None);
    assert_eq!(to_metric_unit(Metric::SocPct, Some("W"), 1.0), None);
    assert_eq!(to_metric_unit(Metric::PvW, None, 1.0), None);
}

#[test]
fn text_states_are_not_stored_but_are_shown() {
    let entities = [entity("sensor.a", None, None)];
    let mut ingest = ingest(&entities);
    let rows = ingest.on_updates(vec![
        update("sensor.a", "unavailable", None, 100),
        update("sensor.a", "unknown", None, 101),
        update("sensor.a", "on", None, 102),
    ]);
    assert!(rows.is_empty());
    let json = ingest.view.to_json();
    assert_eq!(json["entities"][0]["state"], "on");
    assert_eq!(json["entities"][0]["value"], Value::Null);
}

#[test]
fn numeric_states_become_rows_and_mapped_ones_get_a_metric() {
    let entities = [
        entity("sensor.solar_power", Some(Metric::PvW), None),
        entity("sensor.temp", None, None),
    ];
    let mut ingest = ingest(&entities);
    let rows = ingest.on_updates(vec![
        update("sensor.solar_power", "1.5", Some("kW"), 1000),
        update("sensor.temp", "21.5", Some("°C"), 1000),
        update("sensor.stranger", "7", None, 1000),
    ]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].value, 1.5);
    assert_eq!(rows[0].unit.as_deref(), Some("kW"));
    assert_eq!(rows[0].metric, Some((Metric::PvW, 1500.0)));
    assert_eq!(rows[1].metric, None);
}

#[test]
fn config_unit_overrides_and_a_misfit_is_not_mapped() {
    let entities = [
        entity("sensor.a", Some(Metric::PvW), Some("kW")),
        entity("sensor.b", Some(Metric::PvW), None),
    ];
    let mut ingest = ingest(&entities);
    let rows = ingest.on_updates(vec![
        // Home Assistant says nothing; the config says kW.
        update("sensor.a", "2", None, 1000),
        // No unit anywhere: stored as an entity reading but not as a metric.
        update("sensor.b", "2", None, 1000),
    ]);
    assert_eq!(rows[0].metric, Some((Metric::PvW, 2000.0)));
    assert_eq!(rows[1].value, 2.0);
    assert_eq!(rows[1].metric, None);
}

#[test]
fn at_most_one_row_per_entity_per_fifteen_seconds() {
    let entities = [
        entity("sensor.a", None, None),
        entity("sensor.b", None, None),
    ];
    let mut ingest = ingest(&entities);
    let rows = ingest.on_updates(vec![update("sensor.a", "1", None, 1000)]);
    assert_eq!(rows.len(), 1);
    // Too soon: held, not stored.
    for (ts, v) in [(1003, "2"), (1010, "3")] {
        assert!(
            ingest
                .on_updates(vec![update("sensor.a", v, None, ts)])
                .is_empty()
        );
    }
    // Another entity is not held back by the first.
    assert_eq!(
        ingest
            .on_updates(vec![update("sensor.b", "9", None, 1004)])
            .len(),
        1
    );
    // Not due yet at 1014, due at 1015; the newest held value wins.
    assert!(ingest.flush(1014, false).is_empty());
    let rows = ingest.flush(1015, false);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].ts, rows[0].value), (1010, 3.0));
    // Nothing is held any more.
    assert!(ingest.flush(2000, true).is_empty());
    // A reading 15 s after the last stored one (1010) goes straight through.
    assert_eq!(
        ingest
            .on_updates(vec![update("sensor.a", "4", None, 1025)])
            .len(),
        1
    );
}

#[test]
fn a_forced_flush_writes_everything_held() {
    let entities = [entity("sensor.a", None, None)];
    let mut ingest = ingest(&entities);
    ingest.on_updates(vec![update("sensor.a", "1", None, 1000)]);
    ingest.on_updates(vec![update("sensor.a", "2", None, 1001)]);
    let rows = ingest.flush(1001, true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, 2.0);
}

#[test]
fn a_clock_that_steps_back_does_not_freeze_an_entity() {
    let entities = [entity("sensor.a", None, None)];
    let mut ingest = ingest(&entities);
    ingest.on_updates(vec![update("sensor.a", "1", None, 5000)]);
    assert_eq!(
        ingest
            .on_updates(vec![update("sensor.a", "2", None, 100)])
            .len(),
        1
    );
}

#[test]
fn view_lists_configured_entities_before_any_state() {
    let mut named = entity("sensor.a", Some(Metric::SocPct), Some("%"));
    named.name = Some("Battery".into());
    let entities = [named, entity("sensor.b", None, None)];
    let view = HaView::new(&entities);
    let json = view.to_json();
    assert_eq!(json["connected"], false);
    assert_eq!(json["entities"][0]["name"], "Battery");
    assert_eq!(json["entities"][0]["metric"], "soc_pct");
    assert_eq!(json["entities"][0]["unit"], "%");
    assert_eq!(json["entities"][0]["last_updated"], Value::Null);
    assert_eq!(json["entities"][1]["name"], "sensor.b");
}

#[test]
fn only_one_log_line_per_outage_and_the_pause_grows() {
    let mut health = Health::new();
    let err = anyhow::anyhow!("down");
    let first = health.failed(&err);
    assert!(health.logged);
    let second = health.failed(&err);
    assert!(second > first);
    health.connected();
    assert!(!health.logged);
    assert_eq!(health.failed(&err), first);
}

#[test]
fn the_source_name_is_recognised_exactly() {
    assert!(is_home_assistant_source("home-assistant"));
    assert!(!is_home_assistant_source("home-assistant-mqtt"));
    assert!(!is_home_assistant_source("isolarcloud:1"));
}

async fn pool() -> (tempfile::TempDir, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let pool = crate::db::open(dir.path().join("t.db").to_str().unwrap())
        .await
        .unwrap();
    (dir, pool)
}

fn reading(id: &str, ts: i64, value: f64, metric: Option<(Metric, f64)>) -> Reading {
    Reading {
        entity_id: id.into(),
        ts,
        value,
        unit: Some("kW".into()),
        metric,
    }
}

#[tokio::test]
async fn save_writes_entity_rows_and_mapped_metric_rows() {
    let (_dir, pool) = pool().await;
    let rows = [
        reading("sensor.solar_power", 100, 1.5, Some((Metric::PvW, 1500.0))),
        reading("sensor.solar_power", 200, 2.0, Some((Metric::PvW, 2000.0))),
        reading("sensor.temp", 100, 20.0, None),
    ];
    store::save(&pool, &rows).await.unwrap();
    // Saving again overwrites instead of failing.
    store::save(&pool, &[reading("sensor.temp", 100, 21.0, None)])
        .await
        .unwrap();

    let history = store::history(&pool, "sensor.solar_power", 0, 1000, None)
        .await
        .unwrap();
    assert_eq!(history, [(100, 1.5), (200, 2.0)]);
    let temp = store::history(&pool, "sensor.temp", 0, 1000, None)
        .await
        .unwrap();
    assert_eq!(temp, [(100, 21.0)]);
    // `to` is exclusive.
    assert_eq!(
        store::history(&pool, "sensor.solar_power", 0, 200, None)
            .await
            .unwrap()
            .len(),
        1
    );
    let metric_rows: Vec<(String, String, i64, f64)> =
        sqlx::query_as("SELECT source, metric, ts, value FROM metric_reading ORDER BY ts")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        metric_rows,
        [
            ("home-assistant".into(), "pv_w".into(), 100, 1500.0),
            ("home-assistant".into(), "pv_w".into(), 200, 2000.0),
        ]
    );
    let (ts, value, unit) = store::latest(&pool, "sensor.solar_power")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ts, value, unit.as_deref()), (200, 2.0, Some("kW")));
    assert!(store::latest(&pool, "sensor.nope").await.unwrap().is_none());
}

#[tokio::test]
async fn history_can_average_into_steps() {
    let (_dir, pool) = pool().await;
    let rows: Vec<Reading> = [(0, 1.0), (15, 3.0), (60, 10.0)]
        .iter()
        .map(|&(ts, v)| reading("sensor.a", ts, v, None))
        .collect();
    store::save(&pool, &rows).await.unwrap();
    let out = store::history(&pool, "sensor.a", 0, 120, Some(60))
        .await
        .unwrap();
    assert_eq!(out, [(0, 2.0), (60, 10.0)]);
}

#[tokio::test]
async fn saving_announces_only_mapped_metrics() {
    let (_dir, pool) = pool().await;
    let hub = Hub::default();
    let mut events = hub.subscribe();
    // Unmapped only: no event.
    save_and_announce(&pool, &hub, vec![reading("sensor.t", 100, 1.0, None)])
        .await
        .unwrap();
    assert!(events.try_recv().is_err());
    save_and_announce(
        &pool,
        &hub,
        vec![
            reading("sensor.a", 100, 1.0, Some((Metric::PvW, 1000.0))),
            reading("sensor.a", 130, 1.0, Some((Metric::PvW, 1000.0))),
        ],
    )
    .await
    .unwrap();
    match events.try_recv().unwrap() {
        HubEvent::ReadingsStored {
            source,
            metrics,
            from,
            to,
        } => {
            assert_eq!(source, "home-assistant");
            assert_eq!(metrics, [Metric::PvW]);
            assert_eq!((from, to), (100, 130));
        }
        other => panic!("{other:?}"),
    }
}

/// The whole path without a network: the messages of a real login, then the
/// rows that would be stored.
#[test]
fn a_full_conversation_ends_in_rows() {
    let config: Config = toml::from_str(
        "[home_assistant.source]\nurl = \"http://ha:8123\"\ntoken = \"t\"\n\
         [[home_assistant.source.entity]]\nentity_id = \"sensor.solar_power\"\nmetric = \"pv_w\"\n",
    )
    .unwrap();
    let source = config.home_assistant.unwrap().source.unwrap();
    let mut ingest = ingest(&source.entity);
    let mut session = Session::new("t", vec!["sensor.solar_power".into()]);
    let mut rows = Vec::new();
    for text in [
        r#"{"type":"auth_required"}"#,
        r#"{"type":"auth_ok"}"#,
        r#"{"id":2,"type":"result","success":true,"result":[
            {"entity_id":"sensor.solar_power","state":"900","attributes":{"unit_of_measurement":"W"},
             "last_updated":"2026-11-12T00:00:00+00:00"}]}"#,
        r#"{"id":1,"type":"event","event":{"variables":{"trigger":{"to_state":
            {"entity_id":"sensor.solar_power","state":"1.2","attributes":{"unit_of_measurement":"kW"},
             "last_updated":"2026-11-12T00:00:30+00:00"}}}}}"#,
    ] {
        for step in session.on_text(text, 0).unwrap() {
            if let Step::Updates(updates) = step {
                rows.extend(ingest.on_updates(updates));
            }
        }
    }
    let metrics: Vec<_> = rows.iter().map(|r| r.metric.unwrap().1).collect();
    assert_eq!(metrics, [900.0, 1200.0]);
}

/// The real connection code against a scripted Home Assistant on localhost
/// (plain `ws://`, so no certificates): login, subscribe, states, one event.
/// Also checks that the client sends nothing but read commands.
#[tokio::test]
async fn connection_talks_to_a_scripted_home_assistant() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut received = Vec::new();
        ws.send(Message::text(r#"{"type":"auth_required"}"#))
            .await
            .unwrap();
        // auth
        received.push(
            ws.next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap()
                .to_string(),
        );
        ws.send(Message::text(r#"{"type":"auth_ok"}"#))
            .await
            .unwrap();
        // subscribe_trigger and get_states
        for _ in 0..2 {
            received.push(
                ws.next()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_text()
                    .unwrap()
                    .to_string(),
            );
        }
        ws.send(Message::text(
            r#"{"id":2,"type":"result","success":true,"result":[
                {"entity_id":"sensor.solar_power","state":"1.5","attributes":{"unit_of_measurement":"kW"},
                 "last_updated":"2026-11-12T00:00:00+00:00"}]}"#,
        ))
        .await
        .unwrap();
        ws.send(Message::text(
            r#"{"id":1,"type":"event","event":{"variables":{"trigger":{"to_state":
                {"entity_id":"sensor.solar_power","state":"2.5","attributes":{"unit_of_measurement":"kW"},
                 "last_updated":"2026-11-12T00:01:00+00:00"}}}}}"#,
        ))
        .await
        .unwrap();
        received
    });

    let (_dir, pool) = pool().await;
    let entities = vec![entity("sensor.solar_power", Some(Metric::PvW), None)];
    let config = HaSource {
        url: "http://unused".into(),
        token: "secret".into(),
        entity: entities.clone(),
        backfill_days: 0,
    };
    let view = HaView::new(&entities);
    let mut ingest = Ingest::new(&entities, view.clone());
    let mut health = Health::new();
    let hub = Hub::default();
    // The server closes after its script, which ends the connection.
    let result = connection(&url, &config, &mut ingest, &pool, &hub, &mut health).await;
    assert!(result.is_err());
    // The forced flush at disconnect writes the held second reading
    // (60 s later it was not held; both are stored).
    let rows = store::history(&pool, "sensor.solar_power", 0, i64::MAX, None)
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.1).collect::<Vec<_>>(), [1.5, 2.5]);
    let metric: Vec<f64> = sqlx::query_scalar(
        "SELECT value FROM metric_reading WHERE source = 'home-assistant' ORDER BY ts",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(metric, [1500.0, 2500.0]);
    assert!(!health.logged);

    let received = server.await.unwrap();
    let kinds: Vec<String> = received
        .iter()
        .map(|text| {
            let v: Value = serde_json::from_str(text).unwrap();
            v["type"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(kinds, ["auth", "subscribe_trigger", "get_states"]);
    assert!(received[0].contains("secret"));
}
