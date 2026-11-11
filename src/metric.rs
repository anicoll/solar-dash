//! The values the dashboard knows about, named by what they mean rather than
//! by the id any one data source gives them. Code outside the source modules
//! (`src/isolar/`, `src/ingest.rs`) talks about a [`Metric`] and never about a
//! source's own point ids.

use serde::{Deserialize, Serialize};

/// How a metric's value behaves over time. This tells the readers of the
/// values what they can do with them (a counter is read by its day's maximum,
/// a power is integrated over time).
// The names, units and kinds are for the API and the metric store, which come
// in the next PRs of the stack; for now only the tests use them.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// An instant reading in watts.
    Power,
    /// Watt-hours so far today. It grows through the day and resets to 0 at
    /// local midnight.
    DayCounterWh,
    /// An instant reading, 0 to 100.
    Percent,
}

/// One kind of value. The serde names (snake_case) are what is stored in the
/// database and shown by the API, so renaming a variant is a data change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Solar generation as the plant reports it.
    PvW,
    /// Solar generation measured on the DC side, before the inverter.
    PvDcW,
    LoadW,
    /// Positive when importing from the grid, negative when exporting.
    GridW,
    /// Battery power. The sign is whatever the source reports.
    BatteryW,
    /// Battery state of charge in percent.
    SocPct,
    PvDayWh,
    GridImportDayWh,
    GridExportDayWh,
    LoadDayWh,
    BatteryChargeDayWh,
    BatteryDischargeDayWh,
    /// Solar energy used by the house itself (not exported or stored).
    SelfConsumedDayWh,
}

#[allow(dead_code)] // see `MetricKind`
impl Metric {
    /// Every metric, in a fixed order.
    pub const ALL: [Metric; 13] = [
        Metric::PvW,
        Metric::PvDcW,
        Metric::LoadW,
        Metric::GridW,
        Metric::BatteryW,
        Metric::SocPct,
        Metric::PvDayWh,
        Metric::GridImportDayWh,
        Metric::GridExportDayWh,
        Metric::LoadDayWh,
        Metric::BatteryChargeDayWh,
        Metric::BatteryDischargeDayWh,
        Metric::SelfConsumedDayWh,
    ];

    /// The stable name, the same as the serde one ("pv_w").
    pub fn name(self) -> &'static str {
        match self {
            Metric::PvW => "pv_w",
            Metric::PvDcW => "pv_dc_w",
            Metric::LoadW => "load_w",
            Metric::GridW => "grid_w",
            Metric::BatteryW => "battery_w",
            Metric::SocPct => "soc_pct",
            Metric::PvDayWh => "pv_day_wh",
            Metric::GridImportDayWh => "grid_import_day_wh",
            Metric::GridExportDayWh => "grid_export_day_wh",
            Metric::LoadDayWh => "load_day_wh",
            Metric::BatteryChargeDayWh => "battery_charge_day_wh",
            Metric::BatteryDischargeDayWh => "battery_discharge_day_wh",
            Metric::SelfConsumedDayWh => "self_consumed_day_wh",
        }
    }

    /// The metric with this name, or `None` if there is no such metric.
    pub fn from_name(name: &str) -> Option<Metric> {
        Metric::ALL.into_iter().find(|m| m.name() == name)
    }

    pub fn kind(self) -> MetricKind {
        match self {
            Metric::PvW | Metric::PvDcW | Metric::LoadW | Metric::GridW | Metric::BatteryW => {
                MetricKind::Power
            }
            Metric::SocPct => MetricKind::Percent,
            Metric::PvDayWh
            | Metric::GridImportDayWh
            | Metric::GridExportDayWh
            | Metric::LoadDayWh
            | Metric::BatteryChargeDayWh
            | Metric::BatteryDischargeDayWh
            | Metric::SelfConsumedDayWh => MetricKind::DayCounterWh,
        }
    }

    pub fn unit(self) -> &'static str {
        match self.kind() {
            MetricKind::Power => "W",
            MetricKind::DayCounterWh => "Wh",
            MetricKind::Percent => "%",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for metric in Metric::ALL {
            assert_eq!(Metric::from_name(metric.name()), Some(metric));
        }
        assert_eq!(Metric::from_name("nope"), None);
    }

    #[test]
    fn names_are_unique_and_match_serde() {
        let mut seen = std::collections::HashSet::new();
        for metric in Metric::ALL {
            assert!(
                seen.insert(metric.name()),
                "{} is used twice",
                metric.name()
            );
            let json = serde_json::to_string(&metric).unwrap();
            assert_eq!(json, format!("\"{}\"", metric.name()));
            let back: Metric = serde_json::from_str(&json).unwrap();
            assert_eq!(back, metric);
        }
    }

    #[test]
    fn units_follow_kind() {
        assert_eq!(Metric::PvW.unit(), "W");
        assert_eq!(Metric::SocPct.unit(), "%");
        assert_eq!(Metric::LoadDayWh.unit(), "Wh");
        assert_eq!(Metric::LoadDayWh.kind(), MetricKind::DayCounterWh);
    }

    #[test]
    fn all_lists_every_variant_once() {
        // A new variant must be added to `ALL`: `name()` and `kind()` are
        // exhaustive matches, so the compiler forces those, but not this list.
        let count = Metric::ALL.len();
        let unique: std::collections::HashSet<_> = Metric::ALL.into_iter().collect();
        assert_eq!(unique.len(), count);
    }
}
