//! Backfill of past values from Home Assistant's long-term statistics.
//!
//! Home Assistant's recorder keeps two kinds of statistics for sensors that
//! have a `state_class`: **5-minute** ones for about the last 10 days and
//! **hourly** ones for as long as the database is kept. This module copies
//! them into `ha_reading` (and `metric_reading` for mapped entities) the first
//! time an entity is seen, walking backwards from the oldest live reading to
//! `backfill_days` ago.
//!
//! What is stored per statistics row, at the start of its period:
//!
//! - an entity with a mean (`state_class: measurement`, such as power or
//!   temperature): the **mean** of the period,
//! - a meter (`total` or `total_increasing`, such as energy): the meter's
//!   **state** at the end of the period, the same thing the live source stores
//!   (the entity's own reading, in its own unit). The cumulative `sum` is only
//!   used if the state is missing.
//!
//! Progress lives in `sync_state`, one job per entity (`ha-backfill:<id>`)
//! whose cursor is the oldest moment copied so far. A restart carries on from
//! the cursor; an entity whose cursor has reached the floor is done and is
//! never asked about again (raising `backfill_days` later fetches only the
//! extra older part). Requests are chunked so none is huge.
//!
//! Read-only like the rest of the source: the only commands are
//! `recorder/get_statistics_metadata` and `recorder/statistics_during_period`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use super::{Health, Reading, store, to_metric_unit};
use crate::config::{HaEntity, HaSource};
use crate::db;
use crate::hub::Hub;

/// Home Assistant keeps 5-minute statistics for 10 days by default.
const FIVE_MINUTE_DAYS: i64 = 10;
/// Days per 5-minute request (about 1,440 rows).
const FIVE_MINUTE_CHUNK_DAYS: i64 = 5;
/// Days per hourly request (about 720 rows).
const HOURLY_CHUNK_DAYS: i64 = 30;
/// Pause between requests, to be gentle with Home Assistant.
const PAUSE: Duration = Duration::from_secs(1);
/// Statistics queries can be slow on a big database.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

const DAY: i64 = 24 * 3600;

/// The statistics period to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    FiveMinute,
    Hour,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Period::FiveMinute => "5minute",
            Period::Hour => "hour",
        }
    }
}

/// One request's time range, `start` inclusive and `end` exclusive (unix
/// seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub start: i64,
    pub end: i64,
    pub period: Period,
}

/// The `sync_state` job that holds an entity's progress.
pub fn job_name(entity_id: &str) -> String {
    format!("ha-backfill:{entity_id}")
}

/// The oldest moment to fetch: `days` before `now`.
pub fn floor_ts(now: i64, days: u32) -> i64 {
    now - i64::from(days) * DAY
}

/// The next range to ask for, going backwards from `end`, or `None` when
/// `end` has reached `floor`. Ranges newer than 10 days before `now` use
/// 5-minute statistics, older ones hourly; a range never crosses that line,
/// so each answer has one period.
pub fn plan_chunk(end: i64, floor: i64, now: i64) -> Option<Chunk> {
    if end <= floor {
        return None;
    }
    let boundary = now - FIVE_MINUTE_DAYS * DAY;
    Some(if end > boundary {
        Chunk {
            start: (end - FIVE_MINUTE_CHUNK_DAYS * DAY)
                .max(boundary)
                .max(floor),
            end,
            period: Period::FiveMinute,
        }
    } else {
        Chunk {
            start: (end - HOURLY_CHUNK_DAYS * DAY).max(floor),
            end,
            period: Period::Hour,
        }
    })
}

/// What Home Assistant knows about an entity's statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub unit: Option<String>,
    /// Whether the statistics have a mean (else they are a meter).
    pub has_mean: bool,
}

/// `recorder/get_statistics_metadata` for one entity.
pub fn metadata_request(entity_id: &str) -> Value {
    json!({"type": "recorder/get_statistics_metadata", "statistic_ids": [entity_id]})
}

/// The entity's metadata from the reply, or `None` when Home Assistant keeps
/// no statistics for it (a sensor without a `state_class`).
pub fn parse_metadata(result: &Value, entity_id: &str) -> Option<Meta> {
    let row = result
        .as_array()?
        .iter()
        .find(|row| row["statistic_id"] == entity_id)?;
    Some(Meta {
        unit: row["statistics_unit_of_measurement"]
            .as_str()
            .map(str::to_string),
        has_mean: row["has_mean"].as_bool().unwrap_or(false),
    })
}

/// `recorder/statistics_during_period` for one entity and chunk.
pub fn statistics_request(entity_id: &str, chunk: Chunk) -> Value {
    let iso = |ts: i64| {
        DateTime::from_timestamp(ts, 0)
            .unwrap_or_default()
            .to_rfc3339()
    };
    json!({
        "type": "recorder/statistics_during_period",
        "start_time": iso(chunk.start),
        "end_time": iso(chunk.end),
        "statistic_ids": [entity_id],
        "period": chunk.period.name(),
        "types": ["mean", "state", "sum"],
    })
}

/// The `(ts, value)` pairs of an entity from a `statistics_during_period`
/// reply, oldest first. A period start is milliseconds since the epoch (an
/// ISO text is accepted too). A row without the wanted number is skipped.
pub fn parse_statistics(result: &Value, entity_id: &str, meta: &Meta) -> Vec<(i64, f64)> {
    let Some(rows) = result[entity_id].as_array() else {
        return Vec::new();
    };
    let mut out: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|row| {
            let ts = match &row["start"] {
                Value::Number(ms) => (ms.as_f64()? / 1000.0).floor() as i64,
                Value::String(text) => DateTime::parse_from_rfc3339(text).ok()?.timestamp(),
                _ => return None,
            };
            let number = |key: &str| row[key].as_f64().filter(|v| v.is_finite());
            let value = if meta.has_mean {
                number("mean")
            } else {
                number("state").or_else(|| number("sum"))
            }?;
            Some((ts, value))
        })
        .collect();
    out.sort_by_key(|&(ts, _)| ts);
    out
}

/// Something that answers Home Assistant requests: the real WebSocket in
/// production, a script in tests.
pub trait Requester {
    /// Sends one command (without an `id`) and returns its `result`.
    async fn request(&mut self, message: Value) -> Result<Value>;
}

/// Copies one entity's history, a chunk at a time, until it reaches the
/// floor. Progress is saved after every chunk. Returns how many chunks were
/// fetched.
pub async fn backfill_entity<R: Requester>(
    requester: &mut R,
    pool: &SqlitePool,
    hub: &Hub,
    entity: &HaEntity,
    floor: i64,
    now: i64,
    pause: Duration,
) -> Result<usize> {
    let job = job_name(&entity.entity_id);
    let mut end = match db::get_cursor(pool, &job).await? {
        Some(cursor) => cursor,
        // First time: start where the live readings begin, so the two do not
        // overlap; with no live readings yet, from now.
        None => store::oldest_ts(pool, &entity.entity_id)
            .await?
            .unwrap_or(now),
    };
    if end <= floor {
        return Ok(0);
    }
    let reply = requester
        .request(metadata_request(&entity.entity_id))
        .await?;
    let Some(meta) = parse_metadata(&reply, &entity.entity_id) else {
        // No statistics at all: nothing to copy, and nothing to ask again.
        info!(
            entity = %entity.entity_id,
            "Home Assistant keeps no long-term statistics for this entity \
             (it needs a state_class); no history to copy"
        );
        db::set_cursor(pool, &job, floor).await?;
        return Ok(0);
    };
    // The config's unit wins, like for live readings.
    let unit = entity.unit.clone().or(meta.unit.clone());
    let mut fetched = 0;
    let mut warned = false;
    while let Some(chunk) = plan_chunk(end, floor, now) {
        if fetched > 0 {
            tokio::time::sleep(pause).await;
        }
        let reply = requester
            .request(statistics_request(&entity.entity_id, chunk))
            .await?;
        let rows: Vec<Reading> = parse_statistics(&reply, &entity.entity_id, &meta)
            .into_iter()
            // The request range is half-open; ignore anything outside it.
            .filter(|&(ts, _)| ts >= chunk.start && ts < chunk.end)
            .map(|(ts, value)| {
                let metric = entity.metric.and_then(|metric| {
                    let converted = to_metric_unit(metric, unit.as_deref(), value);
                    if converted.is_none() && !warned {
                        warned = true;
                        warn!(
                            entity = %entity.entity_id,
                            "history unit {:?} does not fit metric {}; copied as the entity's own history only",
                            unit.as_deref().unwrap_or("none"),
                            metric.name()
                        );
                    }
                    converted.map(|converted| (metric, converted))
                });
                Reading {
                    entity_id: entity.entity_id.clone(),
                    ts,
                    value,
                    unit: unit.clone(),
                    metric,
                }
            })
            .collect();
        super::save_and_announce(pool, hub, rows).await?;
        // Saved first, then the cursor: a crash in between only repeats a chunk.
        db::set_cursor(pool, &job, chunk.start).await?;
        debug!(entity = %entity.entity_id, from = chunk.start, to = chunk.end, "copied Home Assistant history");
        end = chunk.start;
        fetched += 1;
    }
    Ok(fetched)
}

/// A logged-in WebSocket that answers [`Requester`] requests.
struct WsRequester {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl WsRequester {
    /// Connects and logs in (`auth_required`, `auth`, `auth_ok`).
    async fn connect(url: &str, token: &str) -> Result<Self> {
        let (socket, _) = timeout(
            Duration::from_secs(15),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .context("timed out connecting to Home Assistant")?
        .context("could not connect to Home Assistant")?;
        let mut this = Self { socket, next_id: 1 };
        let hello = this.read_json().await?;
        anyhow::ensure!(
            hello["type"] == "auth_required",
            "Home Assistant did not ask for a login"
        );
        this.socket
            .send(Message::text(
                json!({"type": "auth", "access_token": token}).to_string(),
            ))
            .await
            .context("could not send to Home Assistant")?;
        let verdict = this.read_json().await?;
        match verdict["type"].as_str() {
            Some("auth_ok") => Ok(this),
            Some("auth_invalid") => bail!(
                "Home Assistant refused the access token: {}",
                verdict["message"].as_str().unwrap_or("no reason given")
            ),
            _ => bail!("unexpected reply to the login"),
        }
    }

    /// The next JSON text message.
    async fn read_json(&mut self) -> Result<Value> {
        loop {
            let message = timeout(REPLY_TIMEOUT, self.socket.next())
                .await
                .context("no reply from Home Assistant")?
                .context("Home Assistant closed the connection")?
                .context("Home Assistant connection error")?;
            match message {
                Message::Text(text) => {
                    return serde_json::from_str(&text)
                        .context("Home Assistant sent text that is not JSON");
                }
                Message::Close(_) => bail!("Home Assistant closed the connection"),
                _ => {}
            }
        }
    }
}

impl Requester for WsRequester {
    async fn request(&mut self, mut message: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        message["id"] = json!(id);
        self.socket
            .send(Message::text(message.to_string()))
            .await
            .context("could not send to Home Assistant")?;
        loop {
            let reply = self.read_json().await?;
            if reply["type"] != "result" || reply["id"] != id {
                continue;
            }
            if reply["success"] != true {
                bail!(
                    "Home Assistant refused a history request: {}",
                    reply
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given")
                );
            }
            return Ok(reply["result"].clone());
        }
    }
}

/// Backfills every configured entity, then returns. A failure is retried
/// with a growing pause (logged once per outage); progress is saved, so a
/// retry or a restart resumes where it stopped. Never blocks the live source.
pub async fn run(url: &str, config: &HaSource, pool: &SqlitePool, hub: &Hub) {
    if config.backfill_days == 0 {
        return;
    }
    let mut health = Health::new();
    loop {
        match pass(url, config, pool, hub).await {
            Ok(chunks) => {
                health.connected();
                if chunks > 0 {
                    info!(chunks, "Home Assistant history backfill finished");
                }
                return;
            }
            Err(err) => tokio::time::sleep(health.failed(&err)).await,
        }
    }
}

/// One pass over all entities on a fresh connection.
async fn pass(url: &str, config: &HaSource, pool: &SqlitePool, hub: &Hub) -> Result<usize> {
    let now = Utc::now().timestamp();
    let floor = floor_ts(now, config.backfill_days);
    // Connect only when there is something left to do.
    let mut pending = Vec::new();
    for entity in &config.entity {
        match db::get_cursor(pool, &job_name(&entity.entity_id)).await? {
            Some(cursor) if cursor <= floor => {}
            _ => pending.push(entity),
        }
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let mut requester = WsRequester::connect(url, &config.token).await?;
    let mut chunks = 0;
    for entity in pending {
        chunks += backfill_entity(&mut requester, pool, hub, entity, floor, now, PAUSE).await?;
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::Metric;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn plan_uses_five_minute_for_ten_days_then_hourly() {
        let floor = floor_ts(NOW, 365);
        assert_eq!(floor, NOW - 365 * DAY);
        // The newest chunk: 5 days of 5-minute statistics.
        let first = plan_chunk(NOW, floor, NOW).unwrap();
        assert_eq!(first.period, Period::FiveMinute);
        assert_eq!((first.start, first.end), (NOW - 5 * DAY, NOW));
        let second = plan_chunk(first.start, floor, NOW).unwrap();
        assert_eq!(second.period, Period::FiveMinute);
        assert_eq!((second.start, second.end), (NOW - 10 * DAY, NOW - 5 * DAY));
        // Then hourly, 30 days at a time, never crossing the 10-day line.
        let third = plan_chunk(second.start, floor, NOW).unwrap();
        assert_eq!(third.period, Period::Hour);
        assert_eq!((third.start, third.end), (NOW - 40 * DAY, NOW - 10 * DAY));
        // A chunk that straddles the line is cut at it.
        let odd = plan_chunk(NOW - 8 * DAY, floor, NOW).unwrap();
        assert_eq!(odd.period, Period::FiveMinute);
        assert_eq!(odd.start, NOW - 10 * DAY);
    }

    #[test]
    fn plan_walks_to_the_floor_and_stops() {
        let floor = floor_ts(NOW, 365);
        let mut end = NOW;
        let mut chunks = Vec::new();
        while let Some(chunk) = plan_chunk(end, floor, NOW) {
            assert!(chunk.start < chunk.end);
            assert_eq!(chunk.end, end, "no gap and no overlap");
            end = chunk.start;
            chunks.push(chunk);
        }
        assert_eq!(end, floor);
        // 2 five-minute chunks, then (365 - 10) / 30 rounded up hourly ones.
        assert_eq!(chunks.len(), 2 + 12);
        assert!(plan_chunk(floor, floor, NOW).is_none());
        assert!(plan_chunk(floor - 5, floor, NOW).is_none());
        // A short backfill never asks for more than it needs.
        let short = plan_chunk(NOW, floor_ts(NOW, 2), NOW).unwrap();
        assert_eq!(short.start, NOW - 2 * DAY);
    }

    #[test]
    fn requests_have_the_right_shape() {
        let chunk = Chunk {
            start: 1_700_000_000,
            end: 1_700_086_400,
            period: Period::Hour,
        };
        let request = statistics_request("sensor.solar_power", chunk);
        assert_eq!(request["type"], "recorder/statistics_during_period");
        assert_eq!(request["period"], "hour");
        assert_eq!(request["statistic_ids"], json!(["sensor.solar_power"]));
        assert_eq!(request["start_time"], "2023-11-14T22:13:20+00:00");
        assert_eq!(request["end_time"], "2023-11-15T22:13:20+00:00");
        assert_eq!(
            statistics_request(
                "sensor.x",
                Chunk {
                    period: Period::FiveMinute,
                    ..chunk
                }
            )["period"],
            "5minute"
        );
        assert_eq!(
            metadata_request("sensor.x"),
            json!({"type": "recorder/get_statistics_metadata", "statistic_ids": ["sensor.x"]})
        );
    }

    #[test]
    fn metadata_tells_the_unit_and_mean_or_meter() {
        let reply = json!([
            {"statistic_id": "sensor.other", "has_mean": true},
            {"statistic_id": "sensor.solar_power", "has_mean": true, "has_sum": false,
             "statistics_unit_of_measurement": "W"},
        ]);
        assert_eq!(
            parse_metadata(&reply, "sensor.solar_power"),
            Some(Meta {
                unit: Some("W".into()),
                has_mean: true
            })
        );
        assert_eq!(parse_metadata(&reply, "sensor.gone"), None);
        assert_eq!(parse_metadata(&json!([]), "sensor.x"), None);
    }

    #[test]
    fn measurement_entities_store_the_mean() {
        let meta = Meta {
            unit: None,
            has_mean: true,
        };
        let reply = json!({"sensor.p": [
            {"start": 1_700_000_300_000_i64, "end": 1_700_000_600_000_i64, "mean": 412.5, "state": 1.0, "sum": 9.0},
            {"start": 1_700_000_000_000_i64, "mean": 400.0},
            {"start": 1_700_000_600_000_i64, "mean": null},
        ]});
        // Oldest first; the row without a mean is skipped.
        assert_eq!(
            parse_statistics(&reply, "sensor.p", &meta),
            [(1_700_000_000, 400.0), (1_700_000_300, 412.5)]
        );
    }

    #[test]
    fn meter_entities_store_the_state_and_fall_back_to_the_sum() {
        let meta = Meta {
            unit: None,
            has_mean: false,
        };
        let reply = json!({"sensor.e": [
            {"start": 1_700_000_000_000_i64, "mean": null, "state": 12.5, "sum": 100.0},
            {"start": "2023-11-14T23:00:00+00:00", "state": null, "sum": 7.0},
            {"start": 1_700_007_200_000_i64, "state": null, "sum": null},
            {"start": "garbage", "state": 1.0},
        ]});
        assert_eq!(
            parse_statistics(&reply, "sensor.e", &meta),
            [(1_700_000_000, 12.5), (1_700_002_800, 7.0)]
        );
        // No such entity in the reply: nothing.
        assert!(parse_statistics(&reply, "sensor.zzz", &meta).is_empty());
        assert!(parse_statistics(&json!({}), "sensor.e", &meta).is_empty());
    }

    /// A scripted Home Assistant: records every request and answers with one
    /// 5-minute-or-hourly row at the start of each requested range.
    struct Fake {
        requests: Vec<Value>,
        has_stats: bool,
        fail_on_request: Option<usize>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                requests: Vec::new(),
                has_stats: true,
                fail_on_request: None,
            }
        }
        fn chunks(&self) -> Vec<(String, String)> {
            self.requests
                .iter()
                .filter(|r| r["type"] == "recorder/statistics_during_period")
                .map(|r| {
                    (
                        r["period"].as_str().unwrap().to_string(),
                        r["start_time"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        }
    }

    impl Requester for Fake {
        async fn request(&mut self, message: Value) -> Result<Value> {
            if self.fail_on_request == Some(self.requests.len()) {
                bail!("connection lost");
            }
            self.requests.push(message.clone());
            if message["type"] == "recorder/get_statistics_metadata" {
                return Ok(if self.has_stats {
                    json!([{"statistic_id": "sensor.solar_power", "has_mean": true,
                            "statistics_unit_of_measurement": "kW"}])
                } else {
                    json!([])
                });
            }
            let start = DateTime::parse_from_rfc3339(message["start_time"].as_str().unwrap())
                .unwrap()
                .timestamp();
            Ok(json!({"sensor.solar_power": [
                {"start": start * 1000, "end": (start + 300) * 1000, "mean": 1.5}
            ]}))
        }
    }

    async fn pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let pool = db::open(dir.path().join("t.db").to_str().unwrap())
            .await
            .unwrap();
        (dir, pool)
    }

    fn entity() -> HaEntity {
        HaEntity {
            entity_id: "sensor.solar_power".into(),
            metric: Some(Metric::PvW),
            name: None,
            unit: None,
            invert: false,
        }
    }

    #[tokio::test]
    async fn backfill_copies_chunks_remembers_progress_and_does_not_repeat() {
        let (_dir, pool) = pool().await;
        let hub = Hub::default();
        let floor = floor_ts(NOW, 40);
        let mut fake = Fake::new();
        let n = backfill_entity(
            &mut fake,
            &pool,
            &hub,
            &entity(),
            floor,
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        // 40 days: two 5-minute chunks (10 days) and one hourly (30 days).
        assert_eq!(n, 3);
        let periods: Vec<_> = fake.chunks().into_iter().map(|c| c.0).collect();
        assert_eq!(periods, ["5minute", "5minute", "hour"]);
        assert_eq!(fake.requests[0]["type"], "recorder/get_statistics_metadata");

        // One row per chunk, in the entity's unit and as the metric in W.
        let rows = store::history(&pool, "sensor.solar_power", 0, i64::MAX, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
        let metric: Vec<f64> =
            sqlx::query_scalar("SELECT value FROM metric_reading WHERE source = 'home-assistant'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(metric, [1500.0, 1500.0, 1500.0]);
        // The done marker: the cursor reached the floor.
        assert_eq!(
            db::get_cursor(&pool, &job_name("sensor.solar_power"))
                .await
                .unwrap(),
            Some(floor)
        );

        // Again (a restart): nothing is asked.
        let mut again = Fake::new();
        let n = backfill_entity(
            &mut again,
            &pool,
            &hub,
            &entity(),
            floor,
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert!(again.requests.is_empty());

        // A larger backfill_days later fetches only the older part.
        let mut more = Fake::new();
        let older = floor_ts(NOW, 70);
        backfill_entity(
            &mut more,
            &pool,
            &hub,
            &entity(),
            older,
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(more.chunks().len(), 1);
        assert_eq!(more.chunks()[0].0, "hour");
    }

    #[tokio::test]
    async fn a_failure_keeps_the_progress_and_the_retry_resumes() {
        let (_dir, pool) = pool().await;
        let hub = Hub::default();
        let floor = floor_ts(NOW, 40);
        // Metadata, chunk 1 and chunk 2 work; chunk 3 (request index 3) fails.
        let mut fake = Fake::new();
        fake.fail_on_request = Some(3);
        let err = backfill_entity(
            &mut fake,
            &pool,
            &hub,
            &entity(),
            floor,
            NOW,
            Duration::ZERO,
        )
        .await;
        assert!(err.is_err());
        let cursor = db::get_cursor(&pool, &job_name("sensor.solar_power"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cursor, NOW - 10 * DAY);

        let mut retry = Fake::new();
        backfill_entity(
            &mut retry,
            &pool,
            &hub,
            &entity(),
            floor,
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        // Only the hourly chunk is left.
        assert_eq!(retry.chunks().len(), 1);
        assert_eq!(retry.chunks()[0].0, "hour");
    }

    #[tokio::test]
    async fn first_run_starts_where_the_live_readings_begin() {
        let (_dir, pool) = pool().await;
        let hub = Hub::default();
        let live_start = NOW - DAY;
        store::save(
            &pool,
            &[Reading {
                entity_id: "sensor.solar_power".into(),
                ts: live_start,
                value: 1.0,
                unit: None,
                metric: None,
            }],
        )
        .await
        .unwrap();
        let mut fake = Fake::new();
        backfill_entity(
            &mut fake,
            &pool,
            &hub,
            &entity(),
            floor_ts(NOW, 2),
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        let request = fake
            .requests
            .iter()
            .find(|r| r["type"] == "recorder/statistics_during_period")
            .unwrap();
        let end = DateTime::parse_from_rfc3339(request["end_time"].as_str().unwrap())
            .unwrap()
            .timestamp();
        assert_eq!(end, live_start);
    }

    #[tokio::test]
    async fn an_entity_without_statistics_is_marked_done_after_one_request() {
        let (_dir, pool) = pool().await;
        let hub = Hub::default();
        let floor = floor_ts(NOW, 365);
        let mut fake = Fake::new();
        fake.has_stats = false;
        let n = backfill_entity(
            &mut fake,
            &pool,
            &hub,
            &entity(),
            floor,
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(fake.requests.len(), 1);
        assert_eq!(
            db::get_cursor(&pool, &job_name("sensor.solar_power"))
                .await
                .unwrap(),
            Some(floor)
        );
    }

    #[tokio::test]
    async fn only_read_commands_are_requested() {
        let (_dir, pool) = pool().await;
        let mut fake = Fake::new();
        backfill_entity(
            &mut fake,
            &pool,
            &Hub::default(),
            &entity(),
            floor_ts(NOW, 40),
            NOW,
            Duration::ZERO,
        )
        .await
        .unwrap();
        for request in &fake.requests {
            let kind = request["type"].as_str().unwrap();
            assert!(
                [
                    "recorder/get_statistics_metadata",
                    "recorder/statistics_during_period"
                ]
                .contains(&kind),
                "{kind}"
            );
        }
    }
}
