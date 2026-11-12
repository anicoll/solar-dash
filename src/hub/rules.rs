//! Which source wins when two of them can answer the same question.
//!
//! There are two such questions:
//!
//! - **A day's energy** (`[sources] daily_energy`): from the inverter's live
//!   Modbus samples, from Home Assistant, or from iSolarCloud's daily
//!   counters. The rule used to be hard-coded in the web code ("live samples
//!   if they cover the day well, else the cloud"); it now lives here as data.
//! - **The live view** (`[sources] live`): which source the Today page's
//!   live numbers and the History power chart come from.
//!
//! The defaults give exactly the old behaviour (Modbus, then the cloud for a
//! day; Modbus alone for the live view).
//!
//! # Two kinds of day source
//!
//! The day logic only knows two *kinds* of data, which is what keeps it
//! small:
//!
//! - **Samples** are power readings over time. A day's energy is their
//!   integral. Modbus gives them; so does Home Assistant when its entities
//!   are mapped to power metrics (`pv_w`, `load_w`, ...).
//! - **Counters** are energy-so-far-today values that reset at midnight. A
//!   day's energy is the day's largest value. iSolarCloud gives them; so does
//!   Home Assistant when its entities are mapped to `*_day_wh` metrics.
//!
//! Home Assistant is *one* id in `daily_energy`. It counts as counters when
//! any day counter is mapped (they are exact, so they win over integrating),
//! otherwise as samples.

use anyhow::{Result, bail, ensure};

use super::spokes;
use crate::config::{HaSource, Sources};
use crate::metric::{Metric, MetricKind};

/// The ids that can supply a day's energy, in the default order of
/// preference.
const DAILY_ENERGY_DEFAULT: [&str; 2] = ["sungrow-modbus", "isolarcloud"];
/// Every id `daily_energy` accepts.
const DAILY_ENERGY_VALID: [&str; 3] = ["sungrow-modbus", "home-assistant", "isolarcloud"];
/// Every id `live` accepts.
const LIVE_VALID: [&str; 2] = ["sungrow-modbus", "home-assistant"];
/// A finished day needs this share covered by live samples to win over the
/// cloud counters.
const DEFAULT_FINISHED_DAY_COVERAGE: f64 = 0.98;
/// Today, or a day the cloud has nothing for, needs this share. For today it
/// stays at 0.9 because a stricter bar would flip today between live and
/// cloud numbers as the dongle drops out, and the cloud counters lag anyway.
const DEFAULT_PARTIAL_DAY_COVERAGE: f64 = 0.9;

/// The ids of the three sources.
const MODBUS: &str = "sungrow-modbus";
const CLOUD: &str = "isolarcloud";
const HOME_ASSISTANT: &str = "home-assistant";

/// A source of power samples (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleSource {
    /// The inverter's `live_sample` table.
    Modbus,
    /// Home Assistant's readings of the mapped power metrics.
    HomeAssistant,
}

/// A source of day counters (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterSource {
    Cloud,
    HomeAssistant,
}

/// The day counters that say Home Assistant can supply a day's energy by
/// itself (the other `*_day_wh` metrics only add detail).
const KEY_DAY_COUNTERS: [Metric; 4] = [
    Metric::LoadDayWh,
    Metric::GridImportDayWh,
    Metric::GridExportDayWh,
    Metric::PvDayWh,
];

/// The power and percent metrics that make up a live sample.
pub const SAMPLE_METRICS: [Metric; 5] = [
    Metric::PvW,
    Metric::LoadW,
    Metric::GridW,
    Metric::BatteryW,
    Metric::SocPct,
];

/// Which metrics the user mapped to Home Assistant entities, and which of
/// those need their sign flipped. It is part of the rules (and so of the day
/// cache keys): a day worked out from one mapping must not be reused for
/// another.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HaMapping {
    mapped: Vec<Metric>,
    inverted: Vec<Metric>,
}

impl HaMapping {
    /// The mapping of a `[home_assistant.source]`'s entities.
    pub fn from_source(source: &HaSource) -> Self {
        let mut mapped = Vec::new();
        let mut inverted = Vec::new();
        // In `Metric::ALL` order, so the mapping does not depend on the
        // order of the entities in the file.
        for metric in Metric::ALL {
            if let Some(entity) = source.entity.iter().find(|e| e.metric == Some(metric)) {
                mapped.push(metric);
                if entity.invert {
                    inverted.push(metric);
                }
            }
        }
        Self { mapped, inverted }
    }

    pub fn has(&self, metric: Metric) -> bool {
        self.mapped.contains(&metric)
    }

    /// Does the pages' sign convention need this metric flipped?
    pub fn is_inverted(&self, metric: Metric) -> bool {
        self.inverted.contains(&metric)
    }

    /// Enough to integrate a day: the house load or the grid is mapped (the
    /// other is worked out from it).
    pub fn can_supply_samples(&self) -> bool {
        self.has(Metric::LoadW) || self.has(Metric::GridW)
    }

    /// Enough for a live view: any of the power metrics is mapped.
    pub fn can_supply_live(&self) -> bool {
        SAMPLE_METRICS
            .iter()
            .any(|m| m.kind() == MetricKind::Power && self.has(*m))
    }

    /// Is a key day counter mapped, so a day can be read like the cloud's?
    pub fn can_supply_counters(&self) -> bool {
        KEY_DAY_COUNTERS.iter().any(|m| self.has(*m))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceRules {
    /// Source ids for a day's energy, best first. A source that is left out
    /// is never used.
    daily_energy: Vec<&'static str>,
    /// Sources of the live view, best first.
    live: Vec<SampleSource>,
    /// The Home Assistant mapping, kept only when something uses Home
    /// Assistant (so a home that does not use it keeps its cache keys).
    ha: Option<HaMapping>,
    finished_day_coverage: f64,
    partial_day_coverage: f64,
}

impl Default for SourceRules {
    fn default() -> Self {
        Self {
            daily_energy: DAILY_ENERGY_DEFAULT.to_vec(),
            live: vec![SampleSource::Modbus],
            ha: None,
            finished_day_coverage: DEFAULT_FINISHED_DAY_COVERAGE,
            partial_day_coverage: DEFAULT_PARTIAL_DAY_COVERAGE,
        }
    }
}

/// Checks a list of ids from the config: not empty, each one valid, none
/// twice. `setting` names the list in the error messages.
fn check_ids(
    setting: &str,
    ids: &[String],
    valid: &[&'static str],
    what: &str,
) -> Result<Vec<&'static str>> {
    ensure!(!ids.is_empty(), "{setting} in [sources] must not be empty");
    let mut checked: Vec<&'static str> = Vec::new();
    for id in ids {
        let Some(found) = valid.iter().find(|v| **v == id) else {
            let problem = if spokes::ALL.iter().any(|meta| meta.id == id) {
                format!("is a spoke but does not supply {what}")
            } else {
                "is not a known source id".to_string()
            };
            bail!(
                "{id:?} in {setting} in [sources] {problem}; valid ids: {}",
                valid.join(", ")
            );
        };
        ensure!(
            !checked.contains(found),
            "{id:?} is listed twice in {setting} in [sources]"
        );
        checked.push(found);
    }
    Ok(checked)
}

impl SourceRules {
    /// The defaults with whatever `[sources]` changes. `ha` is the
    /// `[home_assistant.source]` table (if any): listing `home-assistant`
    /// needs it, with entities mapped to what the pages read. Mistakes are
    /// errors that say what is allowed or missing.
    pub fn from_config(sources: &Sources, ha: Option<&HaSource>) -> Result<Self> {
        let mut rules = Self::default();
        if let Some(ids) = &sources.daily_energy {
            rules.daily_energy =
                check_ids("daily_energy", ids, &DAILY_ENERGY_VALID, "daily energy")?;
        }
        if let Some(ids) = &sources.live {
            let checked = check_ids("live", ids, &LIVE_VALID, "the live view")?;
            rules.live = checked
                .into_iter()
                .map(|id| match id {
                    MODBUS => SampleSource::Modbus,
                    _ => SampleSource::HomeAssistant,
                })
                .collect();
        }
        let ha_in_daily = rules.daily_energy.contains(&HOME_ASSISTANT);
        let ha_in_live = rules.live.contains(&SampleSource::HomeAssistant);
        if ha_in_daily || ha_in_live {
            let used_in = if ha_in_daily { "daily_energy" } else { "live" };
            let Some(source) = ha else {
                bail!(
                    "{used_in} in [sources] lists {HOME_ASSISTANT:?} but there is no \
                     [home_assistant.source] table to read it from"
                );
            };
            let mapping = HaMapping::from_source(source);
            if ha_in_daily {
                if mapping.can_supply_counters() {
                    ensure!(
                        !rules.daily_energy.contains(&CLOUD),
                        "daily_energy in [sources] lists both {HOME_ASSISTANT:?} and {CLOUD:?}, \
                         but the Home Assistant entities are mapped to day counters \
                         (*_day_wh), which stand in for the cloud's counters; list only one \
                         of the two (or map power metrics instead of day counters)"
                    );
                } else {
                    ensure!(
                        mapping.can_supply_samples(),
                        "daily_energy in [sources] lists {HOME_ASSISTANT:?} but no entity in \
                         [home_assistant.source] is mapped to load_w or grid_w (to add up the \
                         day from power) or to a day counter (load_day_wh, grid_import_day_wh, \
                         grid_export_day_wh or pv_day_wh)"
                    );
                }
            }
            if ha_in_live {
                ensure!(
                    mapping.can_supply_live(),
                    "live in [sources] lists {HOME_ASSISTANT:?} but no entity in \
                     [home_assistant.source] is mapped to a power metric \
                     (pv_w, load_w, grid_w or battery_w)"
                );
            }
            rules.ha = Some(mapping);
        }
        if let Some(share) = sources.finished_day_coverage {
            rules.finished_day_coverage = share;
        }
        if let Some(share) = sources.partial_day_coverage {
            rules.partial_day_coverage = share;
        }
        ensure!(
            rules.partial_day_coverage > 0.0 && rules.partial_day_coverage <= 1.0,
            "partial_day_coverage in [sources] must be above 0 and at most 1"
        );
        ensure!(
            rules.finished_day_coverage <= 1.0
                && rules.finished_day_coverage >= rules.partial_day_coverage,
            "finished_day_coverage in [sources] must be at most 1 and not below partial_day_coverage ({})",
            rules.partial_day_coverage
        );
        Ok(rules)
    }

    /// Share of a finished day the live samples must cover to be preferred.
    pub fn finished_day_coverage(&self) -> f64 {
        self.finished_day_coverage
    }

    /// Share of today (or of a finished day, as a fallback) the live samples
    /// must cover to be used at all.
    pub fn partial_day_coverage(&self) -> f64 {
        self.partial_day_coverage
    }

    /// The Home Assistant mapping, when Home Assistant is used at all.
    pub fn ha(&self) -> Option<&HaMapping> {
        self.ha.as_ref()
    }

    /// Do the pages read Home Assistant's readings at all (for a day or the
    /// live view)? Then the stored-day fingerprints include them.
    pub fn uses_ha(&self) -> bool {
        self.ha.is_some()
    }

    /// Does Home Assistant take part in the day's energy as day counters?
    fn ha_counters(&self) -> bool {
        self.daily_energy.contains(&HOME_ASSISTANT)
            && self.ha.as_ref().is_some_and(HaMapping::can_supply_counters)
    }

    /// The sample sources a day's energy is integrated from, in order of
    /// preference. Empty when the day only uses counters.
    pub fn daily_sample_order(&self) -> Vec<SampleSource> {
        self.daily_energy
            .iter()
            .filter_map(|id| match *id {
                MODBUS => Some(SampleSource::Modbus),
                HOME_ASSISTANT if !self.ha_counters() => Some(SampleSource::HomeAssistant),
                _ => None,
            })
            .collect()
    }

    /// The sample sources of the live view, in order of preference.
    pub fn live_order(&self) -> &[SampleSource] {
        &self.live
    }

    /// Where the day counters come from, if anywhere.
    pub fn counter_source(&self) -> Option<CounterSource> {
        if self.daily_energy.contains(&CLOUD) {
            Some(CounterSource::Cloud)
        } else if self.ha_counters() {
            Some(CounterSource::HomeAssistant)
        } else {
            None
        }
    }

    /// Does Home Assistant supply the live view?
    pub fn ha_live(&self) -> bool {
        self.live.contains(&SampleSource::HomeAssistant)
    }

    /// Does Home Assistant supply days (history)?
    pub fn ha_daily(&self) -> bool {
        self.daily_energy.contains(&HOME_ASSISTANT)
    }

    /// Are samples a source for daily energy?
    pub fn live_enabled(&self) -> bool {
        !self.daily_sample_order().is_empty()
    }

    /// Are day counters a source for daily energy?
    pub fn counters_enabled(&self) -> bool {
        self.counter_source().is_some()
    }

    /// Is the samples source listed before the counters one (or the only one)?
    fn live_first(&self) -> bool {
        let position =
            |wanted: &dyn Fn(&str) -> bool| self.daily_energy.iter().position(|id| wanted(id));
        let samples = position(&|id| id == MODBUS || (id == HOME_ASSISTANT && !self.ha_counters()));
        let counters = position(&|id| id == CLOUD || (id == HOME_ASSISTANT && self.ha_counters()));
        match (samples, counters) {
            (Some(samples), Some(counters)) => samples < counters,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Should live totals be used instead of the counters'? `live_solid` says
    /// the live samples cover the day well; `counters_have_data` says the
    /// counters have real figures for it. (A live day that is too thin never
    /// gets here.)
    ///
    /// The preferred source wins when it is good enough. A weaker live day
    /// is only a fallback, so a day is not left blank.
    pub fn live_wins(&self, live_solid: bool, counters_have_data: bool) -> bool {
        if !self.counters_enabled() {
            return true;
        }
        if self.live_first() {
            live_solid || !counters_have_data
        } else {
            !counters_have_data
        }
    }

    /// Must the counters be read for a day whose live samples are (or
    /// are not) solid? A solid, preferred live day never looks at them.
    pub fn counters_needed(&self, live_solid: bool) -> bool {
        self.counters_enabled() && !(self.live_first() && live_solid)
    }

    /// How late the counters' readings are stamped, in seconds (see
    /// `web::costs::CLOUD_LAG_SECS`). Home Assistant's are not late.
    pub fn counter_lag_secs(&self, cloud_lag_secs: i64) -> i64 {
        match self.counter_source() {
            Some(CounterSource::HomeAssistant) => 0,
            _ => cloud_lag_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HaEntity;

    fn sources(ids: Option<&[&str]>) -> Sources {
        Sources {
            daily_energy: ids.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            ..Sources::default()
        }
    }

    fn live_sources(ids: &[&str]) -> Sources {
        Sources {
            live: Some(ids.iter().map(|id| id.to_string()).collect()),
            ..Sources::default()
        }
    }

    /// A Home Assistant source with these `(metric, invert)` entities.
    fn ha_source(entities: &[(Metric, bool)]) -> HaSource {
        HaSource {
            url: "http://ha:8123".into(),
            token: "t".into(),
            entity: entities
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
        }
    }

    fn rules(sources: &Sources, ha: Option<&HaSource>) -> Result<SourceRules> {
        SourceRules::from_config(sources, ha)
    }

    const POWER: [(Metric, bool); 2] = [(Metric::GridW, false), (Metric::PvW, false)];
    const COUNTERS: [(Metric, bool); 2] = [(Metric::LoadDayWh, false), (Metric::PvDayWh, false)];

    #[test]
    fn no_sources_section_gives_the_old_behaviour() {
        let rules = rules(&Sources::default(), None).unwrap();
        assert_eq!(rules, SourceRules::default());
        assert_eq!(rules.finished_day_coverage(), 0.98);
        assert_eq!(rules.partial_day_coverage(), 0.9);
        // Solid live wins; partial live only when the cloud has nothing.
        assert!(rules.live_wins(true, true));
        assert!(!rules.live_wins(false, true));
        assert!(rules.live_wins(false, false));
        assert!(!rules.counters_needed(true));
        assert!(rules.counters_needed(false));
        // Modbus alone for the live view, no Home Assistant anywhere.
        assert_eq!(rules.live_order(), [SampleSource::Modbus]);
        assert_eq!(rules.daily_sample_order(), [SampleSource::Modbus]);
        assert_eq!(rules.counter_source(), Some(CounterSource::Cloud));
        assert!(rules.ha().is_none() && !rules.uses_ha());
    }

    #[test]
    fn a_home_assistant_table_that_nothing_uses_changes_nothing() {
        // Mapping entities alone must not change the rules (and so not the
        // cache keys of an install that does not opt in).
        let ha = ha_source(&POWER);
        assert_eq!(
            rules(&Sources::default(), Some(&ha)).unwrap(),
            SourceRules::default()
        );
    }

    #[test]
    fn cloud_first_uses_live_only_when_the_cloud_has_nothing() {
        let rules = rules(&sources(Some(&["isolarcloud", "sungrow-modbus"])), None).unwrap();
        assert!(!rules.live_wins(true, true));
        assert!(rules.live_wins(true, false));
        assert!(rules.counters_needed(true));
    }

    #[test]
    fn a_single_source_is_the_only_one() {
        let live = rules(&sources(Some(&["sungrow-modbus"])), None).unwrap();
        assert!(live.live_enabled() && !live.counters_enabled());
        assert!(live.live_wins(false, true));
        assert!(!live.counters_needed(false));

        let cloud = rules(&sources(Some(&["isolarcloud"])), None).unwrap();
        assert!(!cloud.live_enabled() && cloud.counters_enabled());
        assert!(cloud.counters_needed(true));
    }

    #[test]
    fn mistakes_name_the_valid_ids() {
        let err = rules(&sources(Some(&["solarweb"])), None).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not a known source id"), "{text}");
        assert!(
            text.contains("sungrow-modbus, home-assistant, isolarcloud"),
            "{text}"
        );

        let err = rules(&sources(Some(&["amber"])), None).unwrap_err();
        assert!(format!("{err:#}").contains("does not supply daily energy"));

        assert!(rules(&sources(Some(&[])), None).is_err());
        assert!(rules(&sources(Some(&["isolarcloud", "isolarcloud"])), None).is_err());
        let err = rules(&live_sources(&["isolarcloud"]), None).unwrap_err();
        assert!(format!("{err:#}").contains("live in [sources]"), "{err:#}");
        assert!(rules(&live_sources(&[]), None).is_err());
    }

    #[test]
    fn home_assistant_needs_its_source_and_the_right_mappings() {
        let daily = sources(Some(&["home-assistant"]));
        let err = format!("{:#}", rules(&daily, None).unwrap_err());
        assert!(err.contains("no [home_assistant.source] table"), "{err}");

        // Only a temperature-like metric: nothing to add up a day from.
        let weak = ha_source(&[(Metric::SocPct, false)]);
        let err = format!("{:#}", rules(&daily, Some(&weak)).unwrap_err());
        assert!(err.contains("load_w or grid_w"), "{err}");
        let err = format!(
            "{:#}",
            rules(&live_sources(&["home-assistant"]), Some(&ha_source(&[]))).unwrap_err()
        );
        assert!(err.contains("power metric"), "{err}");

        assert!(rules(&daily, Some(&ha_source(&POWER))).is_ok());
        assert!(rules(&daily, Some(&ha_source(&COUNTERS))).is_ok());
    }

    #[test]
    fn home_assistant_power_is_a_sample_source() {
        let ha = ha_source(&POWER);
        let only = rules(&sources(Some(&["home-assistant"])), Some(&ha)).unwrap();
        assert_eq!(only.daily_sample_order(), [SampleSource::HomeAssistant]);
        assert_eq!(only.counter_source(), None);
        assert!(only.live_enabled() && !only.counters_enabled());
        assert!(only.uses_ha() && only.ha_daily());

        // Modbus first, Home Assistant as the backup, then the cloud.
        let backup = rules(
            &sources(Some(&["sungrow-modbus", "home-assistant", "isolarcloud"])),
            Some(&ha),
        )
        .unwrap();
        assert_eq!(
            backup.daily_sample_order(),
            [SampleSource::Modbus, SampleSource::HomeAssistant]
        );
        assert_eq!(backup.counter_source(), Some(CounterSource::Cloud));
        assert!(backup.live_wins(true, true));
        assert!(!backup.live_wins(false, true));
    }

    #[test]
    fn home_assistant_counters_stand_in_for_the_cloud() {
        let ha = ha_source(&COUNTERS);
        let only = rules(&sources(Some(&["home-assistant"])), Some(&ha)).unwrap();
        assert!(only.daily_sample_order().is_empty());
        assert_eq!(only.counter_source(), Some(CounterSource::HomeAssistant));
        assert!(!only.live_enabled() && only.counters_enabled());
        // Its counters are not late like the cloud's.
        assert_eq!(only.counter_lag_secs(300), 0);
        assert_eq!(SourceRules::default().counter_lag_secs(300), 300);

        // Modbus first, Home Assistant's counters second.
        let backup = rules(
            &sources(Some(&["sungrow-modbus", "home-assistant"])),
            Some(&ha),
        )
        .unwrap();
        assert_eq!(backup.daily_sample_order(), [SampleSource::Modbus]);
        assert_eq!(backup.counter_source(), Some(CounterSource::HomeAssistant));
        assert!(backup.live_wins(true, true) && !backup.live_wins(false, true));

        // Counters and the cloud would fight over the same slot.
        let both = sources(Some(&["home-assistant", "isolarcloud"]));
        let err = format!("{:#}", rules(&both, Some(&ha)).unwrap_err());
        assert!(err.contains("list only one"), "{err}");
    }

    #[test]
    fn the_live_list_orders_the_live_sources() {
        let ha = ha_source(&POWER);
        let r = rules(
            &live_sources(&["home-assistant", "sungrow-modbus"]),
            Some(&ha),
        )
        .unwrap();
        assert_eq!(
            r.live_order(),
            [SampleSource::HomeAssistant, SampleSource::Modbus]
        );
        assert!(r.ha_live());
        // `live` does not touch the day's sources.
        assert_eq!(r.daily_sample_order(), [SampleSource::Modbus]);
    }

    #[test]
    fn the_mapping_is_part_of_the_rules() {
        // So a changed mapping or `invert` makes stored days be worked out again.
        let day = sources(Some(&["home-assistant"]));
        let plain = rules(&day, Some(&ha_source(&POWER))).unwrap();
        let inverted = rules(
            &day,
            Some(&ha_source(&[(Metric::GridW, true), (Metric::PvW, false)])),
        )
        .unwrap();
        assert_ne!(format!("{plain:?}"), format!("{inverted:?}"));
        assert!(inverted.ha().unwrap().is_inverted(Metric::GridW));
        assert!(!inverted.ha().unwrap().is_inverted(Metric::PvW));
    }

    #[test]
    fn thresholds_are_checked() {
        let with = |finished, partial| {
            rules(
                &Sources {
                    finished_day_coverage: finished,
                    partial_day_coverage: partial,
                    ..Sources::default()
                },
                None,
            )
        };
        let rules = with(Some(0.95), Some(0.8)).unwrap();
        assert_eq!(rules.finished_day_coverage(), 0.95);
        assert_eq!(rules.partial_day_coverage(), 0.8);
        assert!(with(Some(1.2), None).is_err());
        assert!(with(None, Some(0.0)).is_err());
        // Finished must not be below partial (the default partial is 0.9).
        assert!(with(Some(0.5), None).is_err());
    }
}
