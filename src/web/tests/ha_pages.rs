//! The pages reading Home Assistant's readings through `[sources]`.
//!
//! Each test seeds Home Assistant readings the way the source stores them
//! (`ha_source::store::save`), builds the rules from a made-up
//! `[home_assistant.source]` table, and checks what the real routes answer.

use super::*;
use crate::config::{HaEntity, HaSource, Sources};
use crate::ha_source::{Reading, store};
use crate::hub::SourceRules;

/// A Home Assistant table with these `(metric, invert)` entities.
fn ha_table(entities: &[(Metric, bool)]) -> HaSource {
    HaSource {
        url: "http://ha:8123".into(),
        token: "t".into(),
        entity: entities
            .iter()
            .map(|(metric, invert)| HaEntity {
                entity_id: format!("sensor.{}", metric.name()),
                metric: Some(*metric),
                name: None,
                unit: None,
                invert: *invert,
            })
            .collect(),
        backfill_days: 0,
    }
}

fn ids(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|id| id.to_string()).collect())
}

/// The rules for these `[sources]` lists and entities.
fn rules(daily: &[&str], live: &[&str], entities: &[(Metric, bool)]) -> SourceRules {
    let sources = Sources {
        daily_energy: (!daily.is_empty()).then(|| ids(daily)).flatten(),
        live: (!live.is_empty()).then(|| ids(live)).flatten(),
        ..Sources::default()
    };
    SourceRules::from_config(&sources, Some(&ha_table(entities))).unwrap()
}

/// A router (plant 0, so no cloud) with these rules, a flat tariff and the
/// Home Assistant spoke on, over an empty database.
async fn app_with(rules: SourceRules, plant: i64) -> (tempfile::TempDir, Router, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(dir.path().join("test.db").to_str().unwrap())
        .await
        .unwrap();
    let hash = hash_password(PASSWORD).unwrap();
    let tz = chrono_tz::Australia::Adelaide;
    let state = AppState::new(pool.clone(), plant, tz, &hash, Key::generate())
        .unwrap()
        .with_tariff(Some(flat_tariff()))
        .with_enabled_spokes(&["home-assistant"])
        .with_source_rules(rules);
    (dir, router(state), pool)
}

/// Stores Home Assistant readings of `metric` at these `(ts, value)` points.
async fn seed(pool: &SqlitePool, metric: Metric, points: &[(i64, f64)]) {
    let readings: Vec<Reading> = points
        .iter()
        .map(|&(ts, value)| Reading {
            entity_id: format!("sensor.{}", metric.name()),
            ts,
            value,
            unit: None,
            metric: Some((metric, value)),
        })
        .collect();
    store::save(pool, &readings).await.unwrap();
}

/// A reading every 5 minutes from `from` up to (not including) `to`.
fn every_5_min(from: i64, to: i64, value: f64) -> Vec<(i64, f64)> {
    (from..to).step_by(300).map(|ts| (ts, value)).collect()
}

async fn get_json(app: &Router, cookie: &str, uri: &str) -> Value {
    body_json(send(app, get_request(uri, Some(cookie))).await).await
}

const DAY: &str = "from=2026-09-15&to=2026-09-15";

#[tokio::test]
async fn a_day_is_integrated_from_home_assistant_power() {
    let rules = rules(
        &["home-assistant"],
        &[],
        &[(Metric::GridW, false), (Metric::LoadW, false)],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let end = SEP_15_START + 86_400;
    seed(
        &pool,
        Metric::GridW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;
    seed(
        &pool,
        Metric::LoadW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;

    let json = get_json(&app, &cookie, &format!("/api/daily?{DAY}")).await;
    let day = &json["days"][0];
    // One row a minute, held for a minute: 1439 steps of 1 kW.
    let expected = 1000.0 * 1439.0 * 60.0 / 3.6e6;
    assert!((day["import_kwh"].as_f64().unwrap() - expected).abs() < 0.01);
    assert!((day["load_kwh"].as_f64().unwrap() - expected).abs() < 0.01);
    // No solar or battery entity: they count as nothing, not as a gap.
    assert_eq!(day["pv_kwh"], 0.0);
    assert_eq!(day["export_kwh"], 0.0);
    assert_eq!(day["battery_charge_kwh"], 0.0);
}

#[tokio::test]
async fn home_assistant_days_follow_the_coverage_rules() {
    let entities = [(Metric::GridW, false), (Metric::LoadW, false)];
    let day_len = 86_400;
    // Readings up to `covered_pct` of the day. A reading is held for an hour
    // (4.2% of the day), which is added to what is covered.
    let import_for = async |covered_pct: i64| {
        let (_dir, app, pool) = app_with(rules(&["home-assistant"], &[], &entities), 0).await;
        let cookie = log_in(&app).await;
        let to = SEP_15_START + day_len * covered_pct / 100;
        for metric in [Metric::GridW, Metric::LoadW] {
            seed(&pool, metric, &every_5_min(SEP_15_START, to, 1000.0)).await;
        }
        let json = get_json(&app, &cookie, &format!("/api/daily?{DAY}")).await;
        json["days"][0]["import_kwh"].clone()
    };
    // 50% + the hold is under the 90% the rules ask for: no figures.
    assert!(import_for(50).await.is_null());
    // 90% + the hold is enough (it is the only source, so partial is used).
    let partial = import_for(90).await.as_f64().unwrap();
    assert!(partial > 20.0 && partial < 24.0, "{partial}");
}

#[tokio::test]
async fn a_day_is_read_from_home_assistant_day_counters() {
    let counters = [
        (Metric::GridImportDayWh, false),
        (Metric::GridExportDayWh, false),
        (Metric::LoadDayWh, false),
        (Metric::PvDayWh, false),
    ];
    let rules = rules(&["home-assistant"], &[], &counters);
    // The cloud plant (7) has a different figure for the day; it is ignored.
    let (_dir, app, pool) = app_with(rules, PLANT).await;
    let cookie = log_in(&app).await;
    db::seed_metrics(
        &pool,
        PLANT,
        &[(Metric::GridImportDayWh, SEP_15_START + 3600, 99_000.0)],
    )
    .await;
    let rising = |peak: f64| {
        vec![
            (SEP_15_START + 3600, peak / 4.0),
            (SEP_15_START + 43_200, peak / 2.0),
            (SEP_15_START + 86_100, peak),
        ]
    };
    seed(&pool, Metric::GridImportDayWh, &rising(5000.0)).await;
    seed(&pool, Metric::GridExportDayWh, &rising(8000.0)).await;
    seed(&pool, Metric::LoadDayWh, &rising(12_000.0)).await;
    seed(&pool, Metric::PvDayWh, &rising(15_000.0)).await;

    let json = get_json(&app, &cookie, &format!("/api/daily?{DAY}")).await;
    let day = &json["days"][0];
    assert_eq!(day["import_kwh"], 5.0);
    assert_eq!(day["export_kwh"], 8.0);
    assert_eq!(day["load_kwh"], 12.0);
    assert_eq!(day["pv_kwh"], 15.0);

    // The costs read the same counters, so an HA-only home has a bill.
    let json = get_json(&app, &cookie, &format!("/api/costs?{DAY}")).await;
    assert_eq!(json["days"][0]["import_kwh"], 5.0);
}

#[tokio::test]
async fn costs_work_for_a_home_that_only_has_home_assistant() {
    let rules = rules(
        &["home-assistant"],
        &["home-assistant"],
        &[(Metric::GridW, false), (Metric::LoadW, false)],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let end = SEP_15_START + 86_400;
    seed(
        &pool,
        Metric::GridW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;
    seed(
        &pool,
        Metric::LoadW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;

    // The same figures as `costs_for_a_live_day` gets from Modbus samples.
    let json = get_json(&app, &cookie, &format!("/api/costs?{DAY}")).await;
    let day = &json["days"][0];
    assert_eq!(day["import_kwh"], 23.98);
    assert_eq!(day["load_kwh"], 23.98);
    assert_eq!(day["actual"], 7.0);
}

#[tokio::test]
async fn modbus_first_and_home_assistant_fills_its_gap() {
    let rules = rules(
        &["sungrow-modbus", "home-assistant"],
        &[],
        &[(Metric::GridW, false), (Metric::LoadW, false)],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let end = SEP_15_START + 86_400;
    let half = SEP_15_START + 43_200;
    // Modbus (2 kW) for the first half only; Home Assistant (1 kW) all day.
    for ts in (SEP_15_START..half).step_by(60) {
        let sample = Sample {
            grid_w: Some(2000.0),
            load_w: Some(2000.0),
            ..live_sample(ts)
        };
        db::insert_live_sample(&pool, &sample).await.unwrap();
    }
    seed(
        &pool,
        Metric::GridW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;
    seed(
        &pool,
        Metric::LoadW,
        &every_5_min(SEP_15_START, end, 1000.0),
    )
    .await;

    let json = get_json(&app, &cookie, &format!("/api/daily?{DAY}")).await;
    let import = json["days"][0]["import_kwh"].as_f64().unwrap();
    // 12 h at 2 kW and 12 h at 1 kW, give or take the two minutes of hold
    // at the join.
    assert!((import - 36.0).abs() < 0.2, "{import}");
}

#[tokio::test]
async fn the_live_view_reads_home_assistant_with_staleness_and_invert() {
    // The grid sensor reports exports as positive: `invert` fixes that.
    let rules = rules(
        &["home-assistant"],
        &["home-assistant"],
        &[
            (Metric::PvW, false),
            (Metric::GridW, true),
            (Metric::BatteryW, false),
            (Metric::SocPct, false),
        ],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let now = db::now();

    // Only an old reading: not "live".
    seed(&pool, Metric::PvW, &[(now - 900, 3000.0)]).await;
    assert!(get_json(&app, &cookie, "/api/live").await["sample"].is_null());

    seed(&pool, Metric::PvW, &[(now - 20, 3000.0)]).await;
    seed(&pool, Metric::GridW, &[(now - 20, -500.0)]).await;
    seed(&pool, Metric::BatteryW, &[(now - 20, 1000.0)]).await;
    seed(&pool, Metric::SocPct, &[(now - 20, 64.0)]).await;
    let json = get_json(&app, &cookie, "/api/live").await;
    let sample = &json["sample"];
    assert_eq!(sample["pv_w"], 3000.0);
    // Raw -500 from the entity is 500 W imported on the dashboard's scale.
    assert_eq!(sample["grid_w"], 500.0);
    assert_eq!(sample["battery_w"], 1000.0);
    assert_eq!(sample["soc_pct"], 64.0);
    // No load entity: solar + grid - battery = 3000 + 500 - 1000.
    assert_eq!(sample["load_w"], 2500.0);
    assert_eq!(json["grid_pull"]["now_w"], 500);
    assert_eq!(json["grid_pull"]["peak_today_w"], 500);

    // Capabilities: the live view and history come from Home Assistant.
    let caps = get_json(&app, &cookie, "/api/capabilities").await;
    assert_eq!(caps["live"], true);
    assert_eq!(caps["history"], true);
    assert_eq!(caps["sources"], serde_json::json!(["home-assistant"]));
}

#[tokio::test]
async fn live_prefers_the_first_fresh_source() {
    let rules = rules(
        &[],
        &["sungrow-modbus", "home-assistant"],
        &[(Metric::GridW, false), (Metric::LoadW, false)],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let now = db::now();
    seed(&pool, Metric::GridW, &[(now - 5, 111.0)]).await;
    seed(&pool, Metric::LoadW, &[(now - 5, 222.0)]).await;

    // Modbus has nothing: Home Assistant answers.
    let json = get_json(&app, &cookie, "/api/live").await;
    assert_eq!(json["sample"]["grid_w"], 111.0);

    // A fresh Modbus sample wins.
    db::insert_live_sample(&pool, &live_sample(now - 10))
        .await
        .unwrap();
    let json = get_json(&app, &cookie, "/api/live").await;
    assert_eq!(json["sample"]["pv_w"], 4895.0);

    // A stale one does not.
    sqlx::query("UPDATE live_sample SET ts = ts - 900")
        .execute(&pool)
        .await
        .unwrap();
    let json = get_json(&app, &cookie, "/api/live").await;
    assert_eq!(json["sample"]["grid_w"], 111.0);
}

#[tokio::test]
async fn the_history_chart_reads_home_assistant_per_minute() {
    let rules = rules(&[], &["home-assistant"], &[(Metric::PvW, false)]);
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    seed(&pool, Metric::PvW, &[(SEP_15_START, 1500.0)]).await;
    let uri = format!(
        "/api/live/series?from={SEP_15_START}&to={}",
        SEP_15_START + 299
    );
    let json = get_json(&app, &cookie, &uri).await;
    let rows = json["data"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0],
        serde_json::json!([SEP_15_START, 1500.0, null, null, null, null])
    );
}

#[tokio::test]
async fn an_ha_only_config_survives_the_route_sweep() {
    let rules = rules(
        &["home-assistant"],
        &["home-assistant"],
        &[
            (Metric::GridW, false),
            (Metric::LoadW, false),
            (Metric::PvW, false),
        ],
    );
    let (_dir, app, pool) = app_with(rules, 0).await;
    let cookie = log_in(&app).await;
    let now = db::now();
    for metric in [Metric::GridW, Metric::LoadW, Metric::PvW] {
        seed(&pool, metric, &every_5_min(now - 3 * 86_400, now, 800.0)).await;
    }
    for uri in [
        "/api/live",
        "/api/live/series?from=1767225600&to=1767312000",
        "/api/capabilities",
        "/api/day",
        "/api/daily",
        "/api/daily?from=2026-01-01&to=2026-02-01",
        "/api/daily?group=month",
        "/api/battery",
        "/api/costs",
        "/api/costs?group=month",
        "/api/bill/current",
        "/api/bill/compare-dynamic",
        "/api/bill/amber-whatif",
        "/api/tariff",
        "/api/globird/compare",
        "/api/payback",
        "/api/ev/sessions?from=1767225600&to=1767312000",
    ] {
        let response = send(&app, get_request(uri, Some(&cookie))).await;
        let status = response.status();
        let body = body_text(response).await;
        assert!(status.as_u16() < 500, "GET {uri} -> {status}: {body}");
    }
    // Today has real figures from Home Assistant.
    let today = get_json(&app, &cookie, "/api/daily").await;
    assert!(
        today["days"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["import_kwh"].is_number())
    );
}

#[tokio::test]
async fn switching_the_rules_changes_the_stored_day_keys() {
    // The rules (with the mapping and `invert`) are part of the keys of the
    // stored days, so a changed preference or sign is worked out again.
    let entities = [(Metric::GridW, false), (Metric::LoadW, false)];
    let plain = rules(&["home-assistant"], &[], &entities);
    let flipped = rules(
        &["home-assistant"],
        &[],
        &[(Metric::GridW, true), (Metric::LoadW, false)],
    );
    let modbus_first = rules(&["sungrow-modbus", "home-assistant"], &[], &entities);
    let keys: std::collections::HashSet<String> =
        [plain, flipped, modbus_first, SourceRules::default()]
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
    assert_eq!(keys.len(), 4);
}

/// The same seeded install for the "unchanged" test: Modbus samples for
/// 2026-09-15, cloud counters for 2026-09-16 and one fresh live sample.
/// Returns the router, and the fresh sample's time.
async fn seeded_install(rules: Option<SourceRules>) -> (tempfile::TempDir, Router, i64) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::open(dir.path().join("test.db").to_str().unwrap())
        .await
        .unwrap();
    for (i, ts) in (SEP_15_START..SEP_15_START + 86_400)
        .step_by(60)
        .enumerate()
    {
        let i = i as i64;
        let pv = ((i % 7) * 300) as f64;
        let load = (500 + (i % 5) * 100) as f64;
        let sample = Sample {
            ts,
            pv_w: Some(pv),
            load_w: Some(load),
            grid_w: Some(load - pv),
            battery_w: Some((100 * (i % 3) - 100) as f64),
            soc_pct: Some(50.0),
        };
        db::insert_live_sample(&pool, &sample).await.unwrap();
    }
    let day2 = SEP_15_START + 86_400;
    let mut readings = Vec::new();
    for (metric, wh) in [
        (Metric::GridImportDayWh, 4000.0),
        (Metric::GridExportDayWh, 7000.0),
        (Metric::LoadDayWh, 11_000.0),
        (Metric::PvDayWh, 14_000.0),
    ] {
        for (n, offset) in [3600, 36_000, 64_800, 85_800].into_iter().enumerate() {
            readings.push((metric, day2 + offset, wh * (n + 1) as f64 / 4.0));
        }
    }
    db::seed_metrics(&pool, PLANT, &readings).await;
    let fresh_ts = db::now() - 10;
    db::insert_live_sample(&pool, &live_sample(fresh_ts))
        .await
        .unwrap();
    let hash = hash_password(PASSWORD).unwrap();
    let tz = chrono_tz::Australia::Adelaide;
    let mut state = AppState::new(pool, PLANT, tz, &hash, Key::generate())
        .unwrap()
        .with_tariff(Some(flat_tariff()));
    if let Some(rules) = rules {
        state = state.with_source_rules(rules);
    }
    (dir, router(state), fresh_ts)
}

/// The pages the "unchanged" test compares.
fn unchanged_uris() -> Vec<String> {
    vec![
        "/api/daily?from=2026-09-15&to=2026-09-17".to_string(),
        "/api/daily?from=2026-09-01&to=2026-09-17&group=month".to_string(),
        "/api/costs?from=2026-09-15&to=2026-09-17".to_string(),
        "/api/battery?from=2026-09-15&to=2026-09-16".to_string(),
        format!(
            "/api/live/series?from={SEP_15_START}&to={}",
            SEP_15_START + 3600
        ),
        "/api/capabilities".to_string(),
        "/api/live".to_string(),
    ]
}

/// What `main` answered for [`unchanged_uris`] on [`seeded_install`], before
/// Home Assistant could be a source (captured from that code, with the fresh
/// sample's time replaced by `{TS}`).
const GOLDEN: &str = include_str!("golden_unchanged.txt");

/// Every page's answer for the seeded install, in the golden file's format.
async fn answers(rules: Option<SourceRules>) -> String {
    let (_dir, app, fresh_ts) = seeded_install(rules).await;
    let cookie = log_in(&app).await;
    let mut out = String::new();
    for uri in unchanged_uris() {
        let body = body_text(send(&app, get_request(&uri, Some(&cookie))).await).await;
        let body = body.replace(&fresh_ts.to_string(), "{TS}");
        out.push_str(&format!("=====URI {uri}\n{body}\n"));
    }
    out
}

#[tokio::test]
async fn an_install_without_home_assistant_answers_exactly_as_before() {
    // The default rules (no `[sources]`, no Home Assistant): byte for byte
    // what `main` answered.
    assert_eq!(answers(None).await, GOLDEN);
    // The same when the config has a Home Assistant table that no rule uses.
    let unused = SourceRules::from_config(
        &Sources::default(),
        Some(&ha_table(&[(Metric::GridW, false), (Metric::LoadW, false)])),
    )
    .unwrap();
    assert_eq!(answers(Some(unused)).await, GOLDEN);
}
