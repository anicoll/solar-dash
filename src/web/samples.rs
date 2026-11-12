//! Where the pages get their power samples from.
//!
//! Every page that reads samples (the live view, the History chart, a day's
//! energy, the costs built on it) asks this module instead of the
//! `live_sample` table. The `[sources]` rules say which sources to use and
//! in what order (see `hub::rules`), so Home Assistant's readings (see
//! `ha_source::samples`) take the same path the Modbus samples always have.
//!
//! With more than one source the first is the main one and the others only
//! **fill its gaps**: a row of a later source is used where the earlier
//! sources have no sample nearby. A day with Modbus and Home Assistant is so
//! Modbus wherever the dongle answered, and Home Assistant where it did not.
//! With the default rules there is one source, Modbus, and every call goes
//! straight to the same query as before.

use anyhow::Result;
use sqlx::SqlitePool;

use super::api::MAX_HOLD_SECS;
use crate::db;
use crate::ha_source::samples as ha_samples;
use crate::hub::{CounterSource, SampleSource, SourceRules};
use crate::modbus::Sample;

/// What the samples are for. It picks the rules' list of sources and how a
/// metric that is not mapped is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    /// The live view and the History chart (`[sources] live`).
    Live,
    /// A day's integrated energy and what is built on it
    /// (`[sources] daily_energy`).
    Daily,
}

fn order(rules: &SourceRules, role: Role) -> Vec<SampleSource> {
    match role {
        Role::Live => rules.live_order().to_vec(),
        Role::Daily => rules.daily_sample_order(),
    }
}

/// Adds the rows of `later` that fall in a gap of `main`: those with no row
/// of `main` within the hold time before them or the next grid step after.
/// Both are sorted by time; so is the result.
fn fill_gaps(main: Vec<Sample>, later: Vec<Sample>) -> Vec<Sample> {
    if main.is_empty() {
        return later;
    }
    let mut out = main;
    let main_len = out.len();
    let extra: Vec<Sample> = later
        .into_iter()
        .filter(|row| {
            // The first main row at or after the start of the hold window.
            let first = out[..main_len].partition_point(|s| s.ts < row.ts - MAX_HOLD_SECS);
            // It is in the way if it is also before the end of this row's step.
            !out[..main_len]
                .get(first)
                .is_some_and(|s| s.ts < row.ts + ha_samples::GRID_SECS)
        })
        .collect();
    out.extend(extra);
    out.sort_by_key(|s| s.ts);
    out
}

/// One source's rows of `[from, to)`: `per_minute` asks Modbus for 1-minute
/// averages instead of every sample (Home Assistant's rows are per minute
/// already).
async fn read_source(
    pool: &SqlitePool,
    rules: &SourceRules,
    role: Role,
    source: SampleSource,
    per_minute: bool,
    from: i64,
    to: i64,
) -> Result<Vec<Sample>> {
    match source {
        SampleSource::Modbus if per_minute => db::live_samples_in_range(pool, from, to).await,
        SampleSource::Modbus => db::live_samples_raw(pool, from, to).await,
        SampleSource::HomeAssistant => match rules.ha() {
            Some(ha) => ha_samples::load(pool, ha, from, to, role == Role::Daily).await,
            None => Ok(Vec::new()),
        },
    }
}

/// Every sample with `from <= ts < to`, oldest first, not averaged (a Home
/// Assistant row is one per minute). The sources of `role` are merged (see
/// the module docs).
pub(super) async fn raw(
    pool: &SqlitePool,
    rules: &SourceRules,
    role: Role,
    from: i64,
    to: i64,
) -> Result<Vec<Sample>> {
    let mut rows = Vec::new();
    for source in order(rules, role) {
        let more = read_source(pool, rules, role, source, false, from, to).await?;
        rows = fill_gaps(rows, more);
    }
    Ok(rows)
}

/// The samples with `from <= ts < to` averaged per minute, oldest first.
pub(super) async fn per_minute(
    pool: &SqlitePool,
    rules: &SourceRules,
    role: Role,
    from: i64,
    to: i64,
) -> Result<Vec<Sample>> {
    let mut rows = Vec::new();
    for source in order(rules, role) {
        let more = read_source(pool, rules, role, source, true, from, to).await?;
        rows = fill_gaps(rows, more);
    }
    Ok(rows)
}

/// The oldest and newest sample time over the sources of `role`, or `None`
/// if there are no samples. It lets a day without samples skip its queries.
pub(super) async fn ts_range(
    pool: &SqlitePool,
    rules: &SourceRules,
    role: Role,
) -> Result<Option<(i64, i64)>> {
    let mut range: Option<(i64, i64)> = None;
    for source in order(rules, role) {
        let found = match source {
            SampleSource::Modbus => db::live_ts_range(pool).await?,
            SampleSource::HomeAssistant => match rules.ha() {
                Some(ha) => ha_samples::ts_range(pool, ha).await?,
                None => None,
            },
        };
        range = match (range, found) {
            (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
            (a, b) => a.or(b),
        };
    }
    Ok(range)
}

/// The `metric_reading` source the day counters are read from: the cloud
/// plant's, or Home Assistant's when its day counters stand in for them (see
/// `hub::rules`).
pub(crate) fn counter_source_name(rules: &SourceRules, plant_id: i64) -> String {
    match rules.counter_source() {
        Some(CounterSource::HomeAssistant) => crate::ha_source::SOURCE_NAME.to_string(),
        _ => crate::isolar::points::source(plant_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ts: i64) -> Sample {
        Sample {
            ts,
            pv_w: Some(1.0),
            load_w: Some(1.0),
            grid_w: Some(1.0),
            battery_w: Some(1.0),
            soc_pct: None,
        }
    }

    #[test]
    fn later_sources_only_fill_gaps() {
        // Main rows every 15 s up to 1000, then nothing until 2000.
        let main: Vec<Sample> = (0..=66).map(|i| at(i * 15)).collect();
        let later: Vec<Sample> = (0..40).map(|i| at(i * 60)).collect();
        let merged = fill_gaps(main, later);
        let times: Vec<i64> = merged.iter().map(|s| s.ts).collect();
        // Sorted, and no later row inside the covered part.
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
        assert!(!times[..67].iter().any(|t| t % 15 != 0));
        // The gap after the last main row (990) is filled from 1140 on
        // (990 + the hold of 120 = 1110, first grid row after is 1140).
        assert!(times.contains(&1140));
        assert!(!times.contains(&1080));
        assert!(times.contains(&2280));
    }

    #[test]
    fn an_empty_main_source_gives_the_later_one() {
        let later = vec![at(60), at(120)];
        assert_eq!(fill_gaps(Vec::new(), later.clone()), later);
    }
}
