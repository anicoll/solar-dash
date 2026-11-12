//! Home Assistant's readings as **samples**: the same `Sample` rows the
//! Modbus poller writes to `live_sample`, so the pages (the live view, the
//! History chart, a day's integrated energy and the costs built on it) can
//! read them through the one code path they already have.
//!
//! The readings of the mapped metrics (`pv_w`, `load_w`, `grid_w`,
//! `battery_w`, `soc_pct`) sit in `metric_reading` under the source name
//! `home-assistant`, one row per entity per change. Samples are made from
//! them at read time:
//!
//! - **Signs.** The pages expect grid power positive when importing and
//!   battery power positive when charging. An entity with `invert = true` is
//!   flipped here, on read, so the stored readings stay as Home Assistant
//!   reported them and fixing the option fixes the past too.
//! - **A regular grid.** Home Assistant only sends a value when it changes,
//!   and each entity changes at its own times. For a range, one row is made
//!   every [`GRID_SECS`], holding each metric's latest reading (see
//!   [`HOLD_SECS`]). That is what the integrator expects (a value held until
//!   the next row) and makes the coverage rules honest: a stretch with no
//!   reading to hold has no row, so it adds no energy and no covered time.
//! - **Metrics that are not mapped.** For a day's energy, a missing solar or
//!   battery metric means the home has none, so it counts as 0 W (the
//!   coverage test needs every column). A missing load or grid is worked out
//!   from the others (`load = solar + grid - battery`). The live view leaves
//!   an unmapped solar or battery blank instead of showing a made-up 0.

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use super::SOURCE_NAME;
use crate::db;
use crate::hub::HaMapping;
use crate::hub::rules::SAMPLE_METRICS;
use crate::metric::Metric;
use crate::modbus::Sample;

/// The spacing of the rows made for a range, in seconds. Equal to the
/// per-minute averaging of the History chart.
pub const GRID_SECS: i64 = 60;

/// How long a reading is held when nothing newer arrived, in seconds. An
/// entity that does not change (a battery idling at 0 W) sends nothing, so
/// the hold must be long; an hour also exactly covers one hourly statistics
/// row copied in by the backfill. A longer outage than this leaves a gap.
pub const HOLD_SECS: i64 = 3600;

/// The live view ignores Home Assistant when its newest reading is older
/// than this many seconds: the numbers would be history, not "now".
pub const LIVE_FRESH_SECS: i64 = 300;

/// A value with the sign flipped, without producing `-0.0` (which would be
/// printed as such in JSON).
fn flip(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { -value }
}

/// One sample from the (already sign-corrected) value of each metric, or
/// `None` when there is nothing at all. `zero_fill` makes an unmapped solar
/// or battery count as 0 W (see the module docs).
pub fn assemble(
    ts: i64,
    value: impl Fn(Metric) -> Option<f64>,
    mapping: &HaMapping,
    zero_fill: bool,
) -> Option<Sample> {
    let mapped_or = |metric: Metric, unmapped: Option<f64>| {
        if mapping.has(metric) {
            value(metric)
        } else {
            unmapped
        }
    };
    let unmapped_power = zero_fill.then_some(0.0);
    let pv_w = mapped_or(Metric::PvW, unmapped_power);
    let battery_w = mapped_or(Metric::BatteryW, unmapped_power);
    // For working out the load or the grid an unmapped metric is 0 whether
    // or not it is shown as such.
    let pv_for_sum = mapped_or(Metric::PvW, Some(0.0));
    let battery_for_sum = mapped_or(Metric::BatteryW, Some(0.0));
    let load_w = if mapping.has(Metric::LoadW) {
        value(Metric::LoadW)
    } else if mapping.has(Metric::GridW) {
        // Everything that comes in (solar, grid) and does not go into the
        // battery is used by the house.
        match (pv_for_sum, value(Metric::GridW), battery_for_sum) {
            (Some(pv), Some(grid), Some(battery)) => Some((pv + grid - battery).max(0.0)),
            _ => None,
        }
    } else {
        None
    };
    let grid_w = if mapping.has(Metric::GridW) {
        value(Metric::GridW)
    } else if mapping.has(Metric::LoadW) {
        match (value(Metric::LoadW), pv_for_sum, battery_for_sum) {
            (Some(load), Some(pv), Some(battery)) => Some(load - pv + battery),
            _ => None,
        }
    } else {
        None
    };
    let soc_pct = mapped_or(Metric::SocPct, None);
    let sample = Sample {
        ts,
        pv_w,
        load_w,
        grid_w,
        battery_w,
        soc_pct,
    };
    [pv_w, load_w, grid_w, battery_w, soc_pct]
        .iter()
        .any(Option::is_some)
        .then_some(sample)
}

/// The grid rows of `[from, to)` from each metric's readings (oldest first,
/// signs already corrected; they may start up to [`HOLD_SECS`] before `from`
/// so the first rows have a value to hold). A pure function of its inputs.
pub fn build_grid(
    readings: &HashMap<Metric, Vec<(i64, f64)>>,
    mapping: &HaMapping,
    from: i64,
    to: i64,
    zero_fill: bool,
) -> Vec<Sample> {
    // How many readings of each metric are at or before the current row.
    let mut seen: HashMap<Metric, usize> = HashMap::new();
    let mut rows = Vec::new();
    // The first multiple of the grid at or after `from`.
    let mut ts = (from + GRID_SECS - 1).div_euclid(GRID_SECS) * GRID_SECS;
    while ts < to {
        let mut held: HashMap<Metric, f64> = HashMap::new();
        for metric in SAMPLE_METRICS {
            let Some(series) = readings.get(&metric) else {
                continue;
            };
            let count = seen.entry(metric).or_insert(0);
            while series.get(*count).is_some_and(|(at, _)| *at <= ts) {
                *count += 1;
            }
            if let Some(&(at, value)) = count.checked_sub(1).and_then(|i| series.get(i))
                && ts - at < HOLD_SECS
            {
                held.insert(metric, value);
            }
        }
        if let Some(sample) = assemble(ts, |m| held.get(&m).copied(), mapping, zero_fill) {
            rows.push(sample);
        }
        ts += GRID_SECS;
    }
    rows
}

/// The metrics of `mapping` a sample is made from.
fn mapped_sample_metrics(mapping: &HaMapping) -> Vec<Metric> {
    SAMPLE_METRICS
        .into_iter()
        .filter(|m| mapping.has(*m))
        .collect()
}

/// The sample rows of `[from, to)`, one per [`GRID_SECS`] (see the module
/// docs for what is held, flipped and filled in).
pub async fn load(
    pool: &SqlitePool,
    mapping: &HaMapping,
    from: i64,
    to: i64,
    zero_fill: bool,
) -> Result<Vec<Sample>> {
    let metrics = mapped_sample_metrics(mapping);
    if metrics.is_empty() || to <= from {
        return Ok(Vec::new());
    }
    let rows =
        db::readings_for_metrics_from(pool, SOURCE_NAME, &metrics, from - HOLD_SECS, to).await?;
    let mut readings: HashMap<Metric, Vec<(i64, f64)>> = HashMap::new();
    for (metric, ts, value) in rows {
        let value = if mapping.is_inverted(metric) {
            flip(value)
        } else {
            value
        };
        readings.entry(metric).or_default().push((ts, value));
    }
    Ok(build_grid(&readings, mapping, from, to, zero_fill))
}

/// The newest reading of one mapped metric as `(ts, value)`, signs corrected.
async fn newest(
    pool: &SqlitePool,
    mapping: &HaMapping,
    metric: Metric,
) -> Result<Option<(i64, f64)>> {
    let row: Option<(i64, f64)> = sqlx::query_as(
        "SELECT ts, value FROM metric_reading
         WHERE source = ? AND metric = ? AND value IS NOT NULL
         ORDER BY ts DESC LIMIT 1",
    )
    .bind(SOURCE_NAME)
    .bind(metric.name())
    .fetch_optional(pool)
    .await
    .context("could not read the newest Home Assistant reading")?;
    Ok(row.map(|(ts, value)| {
        let value = if mapping.is_inverted(metric) {
            flip(value)
        } else {
            value
        };
        (ts, value)
    }))
}

/// The live sample at `now`: each mapped metric's newest reading, or `None`
/// when even the newest one is older than [`LIVE_FRESH_SECS`]. A single
/// metric that has not changed for [`HOLD_SECS`] is left blank.
pub async fn latest(pool: &SqlitePool, mapping: &HaMapping, now: i64) -> Result<Option<Sample>> {
    let mut newest_by_metric: HashMap<Metric, (i64, f64)> = HashMap::new();
    for metric in mapped_sample_metrics(mapping) {
        if let Some(found) = newest(pool, mapping, metric).await? {
            newest_by_metric.insert(metric, found);
        }
    }
    let Some(newest_ts) = newest_by_metric.values().map(|(ts, _)| *ts).max() else {
        return Ok(None);
    };
    if now - newest_ts > LIVE_FRESH_SECS {
        return Ok(None);
    }
    Ok(assemble(
        newest_ts,
        |m| {
            newest_by_metric
                .get(&m)
                .filter(|(ts, _)| now - ts < HOLD_SECS)
                .map(|(_, value)| *value)
        },
        mapping,
        false,
    ))
}

/// The oldest and newest reading time of the mapped sample metrics, the
/// newest moved on by [`HOLD_SECS`] (a reading is held that long). `None`
/// when there is none. Like `db::live_ts_range`, it lets days without data
/// skip their queries.
pub async fn ts_range(pool: &SqlitePool, mapping: &HaMapping) -> Result<Option<(i64, i64)>> {
    let mut range: Option<(i64, i64)> = None;
    for metric in mapped_sample_metrics(mapping) {
        // Two subqueries, so each uses the (source, metric, ts) key.
        let (min, max): (Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT (SELECT MIN(ts) FROM metric_reading WHERE source = ?1 AND metric = ?2),
                    (SELECT MAX(ts) FROM metric_reading WHERE source = ?1 AND metric = ?2)",
        )
        .bind(SOURCE_NAME)
        .bind(metric.name())
        .fetch_one(pool)
        .await
        .context("could not read the Home Assistant reading range")?;
        if let Some((min, max)) = min.zip(max) {
            range = Some(match range {
                Some((a, b)) => (a.min(min), b.max(max)),
                None => (min, max),
            });
        }
    }
    Ok(range.map(|(first, last)| (first, last + HOLD_SECS)))
}

/// The highest grid import (W) from `since` on, as `(ts, import_w)`, like
/// `db::peak_grid_import`. `None` without a mapped grid power.
pub async fn peak_import(
    pool: &SqlitePool,
    mapping: &HaMapping,
    since: i64,
) -> Result<Option<(i64, f64)>> {
    if !mapping.has(Metric::GridW) {
        return Ok(None);
    }
    // An inverted entity imports when its raw value is negative.
    let sql = if mapping.is_inverted(Metric::GridW) {
        "SELECT ts, -value FROM metric_reading
         WHERE source = ? AND metric = ? AND ts >= ? AND value < 0
         ORDER BY value ASC, ts LIMIT 1"
    } else {
        "SELECT ts, value FROM metric_reading
         WHERE source = ? AND metric = ? AND ts >= ? AND value > 0
         ORDER BY value DESC, ts LIMIT 1"
    };
    sqlx::query_as(sql)
        .bind(SOURCE_NAME)
        .bind(Metric::GridW.name())
        .bind(since)
        .fetch_optional(pool)
        .await
        .context("could not read the peak Home Assistant grid import")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HaEntity, HaSource};

    fn mapping(entries: &[(Metric, bool)]) -> HaMapping {
        HaMapping::from_source(&HaSource {
            url: "http://ha:8123".into(),
            token: "t".into(),
            entity: entries
                .iter()
                .enumerate()
                .map(|(i, (metric, invert))| HaEntity {
                    entity_id: format!("sensor.s{i}"),
                    metric: Some(*metric),
                    name: None,
                    unit: None,
                    invert: *invert,
                })
                .collect(),
            backfill_days: 0,
        })
    }

    fn series(rows: &[(i64, f64)]) -> Vec<(i64, f64)> {
        rows.to_vec()
    }

    #[test]
    fn rows_are_made_every_minute_and_hold_the_latest_reading() {
        let map = mapping(&[(Metric::GridW, false), (Metric::LoadW, false)]);
        let readings = HashMap::from([
            (Metric::GridW, series(&[(30, 100.0), (150, 300.0)])),
            (Metric::LoadW, series(&[(0, 500.0)])),
        ]);
        let rows = build_grid(&readings, &map, 0, 240, false);
        let at: Vec<(i64, Option<f64>, Option<f64>)> =
            rows.iter().map(|s| (s.ts, s.grid_w, s.load_w)).collect();
        assert_eq!(
            at,
            [
                (0, None, Some(500.0)),
                (60, Some(100.0), Some(500.0)),
                (120, Some(100.0), Some(500.0)),
                (180, Some(300.0), Some(500.0)),
            ]
        );
    }

    #[test]
    fn a_reading_is_held_for_an_hour_then_the_rows_stop() {
        let map = mapping(&[(Metric::GridW, false), (Metric::LoadW, false)]);
        let readings = HashMap::from([
            (Metric::GridW, series(&[(0, 100.0)])),
            (Metric::LoadW, series(&[(0, 100.0)])),
        ]);
        // 2 hours asked for, only the first hour has a reading to hold.
        let rows = build_grid(&readings, &map, 0, 7200, false);
        assert_eq!(rows.len() as i64, HOLD_SECS / GRID_SECS);
        assert_eq!(rows.last().unwrap().ts, HOLD_SECS - GRID_SECS);
    }

    #[test]
    fn an_earlier_reading_is_carried_into_the_range() {
        let map = mapping(&[(Metric::GridW, false), (Metric::LoadW, false)]);
        let readings = HashMap::from([
            (Metric::GridW, series(&[(-600, 7.0)])),
            (Metric::LoadW, series(&[(-600, 9.0)])),
        ]);
        let rows = build_grid(&readings, &map, 0, 120, false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].grid_w, Some(7.0));
    }

    #[test]
    fn unmapped_solar_and_battery_are_zero_for_a_day_but_blank_for_live() {
        let map = mapping(&[(Metric::GridW, false), (Metric::LoadW, false)]);
        let value = |m: Metric| match m {
            Metric::GridW => Some(400.0),
            Metric::LoadW => Some(400.0),
            _ => None,
        };
        let day = assemble(0, value, &map, true).unwrap();
        assert_eq!((day.pv_w, day.battery_w), (Some(0.0), Some(0.0)));
        let live = assemble(0, value, &map, false).unwrap();
        assert_eq!((live.pv_w, live.battery_w), (None, None));
        assert_eq!(live.soc_pct, None);
    }

    #[test]
    fn a_missing_load_or_grid_is_worked_out_from_the_others() {
        // solar 3000, grid +500 (import), battery +1000 (charging).
        let value = |m: Metric| match m {
            Metric::PvW => Some(3000.0),
            Metric::GridW => Some(500.0),
            Metric::BatteryW => Some(1000.0),
            _ => None,
        };
        let no_load = mapping(&[
            (Metric::PvW, false),
            (Metric::GridW, false),
            (Metric::BatteryW, false),
        ]);
        assert_eq!(
            assemble(0, value, &no_load, false).unwrap().load_w,
            Some(2500.0)
        );

        let value = |m: Metric| match m {
            Metric::PvW => Some(3000.0),
            Metric::LoadW => Some(2500.0),
            Metric::BatteryW => Some(1000.0),
            _ => None,
        };
        let no_grid = mapping(&[
            (Metric::PvW, false),
            (Metric::LoadW, false),
            (Metric::BatteryW, false),
        ]);
        assert_eq!(
            assemble(0, value, &no_grid, false).unwrap().grid_w,
            Some(500.0)
        );
    }

    #[test]
    fn flipping_a_sign_never_gives_negative_zero() {
        assert_eq!(flip(0.0).to_string(), "0");
        assert_eq!(flip(-5.0), 5.0);
        assert_eq!(flip(5.0), -5.0);
    }
}
