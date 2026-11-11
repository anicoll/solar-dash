//! Which source wins when two of them can answer the same question.
//!
//! Today there is one such question: a day's energy totals can come from the
//! inverter's live Modbus samples or from iSolarCloud's daily counters. The
//! rule used to be hard-coded in the web code ("live samples if they cover
//! the day well, else the cloud"). It now lives here as data, so it can be
//! changed in the `[sources]` config section. The defaults give exactly the
//! old behaviour.

use anyhow::{Result, bail, ensure};

use super::spokes;
use crate::config::Sources;

/// The spokes that can supply a day's energy, in the default order of
/// preference. The first one is preferred when it covers the day well.
const DAILY_ENERGY_DEFAULT: [&str; 2] = ["sungrow-modbus", "isolarcloud"];
/// A finished day needs this share covered by live samples to win over the
/// cloud counters.
const DEFAULT_FINISHED_DAY_COVERAGE: f64 = 0.98;
/// Today, or a day the cloud has nothing for, needs this share. For today it
/// stays at 0.9 because a stricter bar would flip today between live and
/// cloud numbers as the dongle drops out, and the cloud counters lag anyway.
const DEFAULT_PARTIAL_DAY_COVERAGE: f64 = 0.9;

/// The id of the live (Modbus) source and of the cloud one.
const LIVE: &str = "sungrow-modbus";
const CLOUD: &str = "isolarcloud";

#[derive(Debug, Clone, PartialEq)]
pub struct SourceRules {
    /// Source ids for a day's energy, best first. A source that is left out
    /// is never used.
    daily_energy: Vec<&'static str>,
    finished_day_coverage: f64,
    partial_day_coverage: f64,
}

impl Default for SourceRules {
    fn default() -> Self {
        Self {
            daily_energy: DAILY_ENERGY_DEFAULT.to_vec(),
            finished_day_coverage: DEFAULT_FINISHED_DAY_COVERAGE,
            partial_day_coverage: DEFAULT_PARTIAL_DAY_COVERAGE,
        }
    }
}

impl SourceRules {
    /// The defaults with whatever `[sources]` changes. Mistakes are errors
    /// that say what is allowed.
    pub fn from_config(sources: &Sources) -> Result<Self> {
        let mut rules = Self::default();
        if let Some(ids) = &sources.daily_energy {
            ensure!(
                !ids.is_empty(),
                "daily_energy in [sources] must not be empty"
            );
            let mut checked: Vec<&'static str> = Vec::new();
            for id in ids {
                let Some(valid) = DAILY_ENERGY_DEFAULT.iter().find(|valid| **valid == id) else {
                    let known = spokes::ALL.iter().any(|meta| meta.id == id);
                    let problem = if known {
                        "is a spoke but does not supply daily energy"
                    } else {
                        "is not a known source id"
                    };
                    bail!(
                        "{id:?} in daily_energy in [sources] {problem}; valid ids: {}",
                        DAILY_ENERGY_DEFAULT.join(", ")
                    );
                };
                ensure!(
                    !checked.contains(valid),
                    "{id:?} is listed twice in daily_energy in [sources]"
                );
                checked.push(valid);
            }
            rules.daily_energy = checked;
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

    /// Are the live samples a source for daily energy?
    pub fn live_enabled(&self) -> bool {
        self.daily_energy.contains(&LIVE)
    }

    /// Are the cloud counters a source for daily energy?
    pub fn cloud_enabled(&self) -> bool {
        self.daily_energy.contains(&CLOUD)
    }

    /// Is the live source listed before the cloud one (or the only one)?
    fn live_first(&self) -> bool {
        match (
            self.daily_energy.iter().position(|id| *id == LIVE),
            self.daily_energy.iter().position(|id| *id == CLOUD),
        ) {
            (Some(live), Some(cloud)) => live < cloud,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Should live totals be used instead of the cloud's? `live_solid` says
    /// the live samples cover the day well; `cloud_has_data` says the cloud
    /// has real figures for it. (A live day that is too thin never gets here.)
    ///
    /// The preferred source wins when it is good enough. A weaker live day
    /// is only a fallback, so a day is not left blank.
    pub fn live_wins(&self, live_solid: bool, cloud_has_data: bool) -> bool {
        if !self.cloud_enabled() {
            return true;
        }
        if self.live_first() {
            live_solid || !cloud_has_data
        } else {
            !cloud_has_data
        }
    }

    /// Must the cloud counters be read for a day whose live samples are (or
    /// are not) solid? A solid, preferred live day never looks at them.
    pub fn cloud_needed(&self, live_solid: bool) -> bool {
        self.cloud_enabled() && !(self.live_first() && live_solid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(ids: Option<&[&str]>) -> Sources {
        Sources {
            daily_energy: ids.map(|ids| ids.iter().map(|id| id.to_string()).collect()),
            ..Sources::default()
        }
    }

    #[test]
    fn no_sources_section_gives_the_old_behaviour() {
        let rules = SourceRules::from_config(&Sources::default()).unwrap();
        assert_eq!(rules, SourceRules::default());
        assert_eq!(rules.finished_day_coverage(), 0.98);
        assert_eq!(rules.partial_day_coverage(), 0.9);
        // Solid live wins; partial live only when the cloud has nothing.
        assert!(rules.live_wins(true, true));
        assert!(!rules.live_wins(false, true));
        assert!(rules.live_wins(false, false));
        assert!(!rules.cloud_needed(true));
        assert!(rules.cloud_needed(false));
    }

    #[test]
    fn cloud_first_uses_live_only_when_the_cloud_has_nothing() {
        let rules =
            SourceRules::from_config(&sources(Some(&["isolarcloud", "sungrow-modbus"]))).unwrap();
        assert!(!rules.live_wins(true, true));
        assert!(rules.live_wins(true, false));
        assert!(rules.cloud_needed(true));
    }

    #[test]
    fn a_single_source_is_the_only_one() {
        let live = SourceRules::from_config(&sources(Some(&["sungrow-modbus"]))).unwrap();
        assert!(live.live_enabled() && !live.cloud_enabled());
        assert!(live.live_wins(false, true));
        assert!(!live.cloud_needed(false));

        let cloud = SourceRules::from_config(&sources(Some(&["isolarcloud"]))).unwrap();
        assert!(!cloud.live_enabled() && cloud.cloud_enabled());
        assert!(cloud.cloud_needed(true));
    }

    #[test]
    fn mistakes_name_the_valid_ids() {
        let err = SourceRules::from_config(&sources(Some(&["solarweb"]))).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not a known source id"), "{text}");
        assert!(text.contains("sungrow-modbus, isolarcloud"), "{text}");

        let err = SourceRules::from_config(&sources(Some(&["amber"]))).unwrap_err();
        assert!(format!("{err:#}").contains("does not supply daily energy"));

        assert!(SourceRules::from_config(&sources(Some(&[]))).is_err());
        assert!(SourceRules::from_config(&sources(Some(&["isolarcloud", "isolarcloud"]))).is_err());
    }

    #[test]
    fn thresholds_are_checked() {
        let with = |finished, partial| {
            SourceRules::from_config(&Sources {
                finished_day_coverage: finished,
                partial_day_coverage: partial,
                ..Sources::default()
            })
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
