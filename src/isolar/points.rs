//! Which iSolarCloud point id means which [`Metric`]. This is the only place
//! (with `ingest.rs` and the config) that knows Sungrow's ids; everything else
//! asks for a metric.

use crate::metric::Metric;

/// One point id and what it means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointMap {
    pub point_id: i64,
    pub metric: Metric,
    /// Multiply the cloud's value by this to get the metric's unit. Only the
    /// battery charge needs it: iSolarCloud stores it as a fraction (0.674)
    /// although its unit says "%".
    pub scale: f64,
}

const fn map(point_id: i64, metric: Metric, scale: f64) -> PointMap {
    PointMap {
        point_id,
        metric,
        scale,
    }
}

/// Every point we understand. The order is the default config's.
pub const POINTS: [PointMap; 13] = [
    map(83033, Metric::PvW, 1.0),
    map(83067, Metric::PvDcW, 1.0),
    map(83106, Metric::LoadW, 1.0),
    map(83328, Metric::GridW, 1.0),
    map(83326, Metric::BatteryW, 1.0),
    map(83252, Metric::SocPct, 100.0),
    map(83022, Metric::PvDayWh, 1.0),
    map(83102, Metric::GridImportDayWh, 1.0),
    map(83072, Metric::GridExportDayWh, 1.0),
    map(83118, Metric::LoadDayWh, 1.0),
    map(83322, Metric::BatteryChargeDayWh, 1.0),
    map(83323, Metric::BatteryDischargeDayWh, 1.0),
    map(83097, Metric::SelfConsumedDayWh, 1.0),
];

/// What a point id means, or `None` for a point we do not know.
pub fn for_point(point_id: i64) -> Option<PointMap> {
    POINTS.into_iter().find(|p| p.point_id == point_id)
}

/// The point that carries a metric. Every metric has exactly one (a test checks).
pub fn for_metric(metric: Metric) -> PointMap {
    POINTS
        .into_iter()
        .find(|p| p.metric == metric)
        .expect("every metric has a point")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_has_exactly_one_point() {
        for metric in Metric::ALL {
            let count = POINTS.iter().filter(|p| p.metric == metric).count();
            assert_eq!(count, 1, "{}", metric.name());
            assert_eq!(for_metric(metric).metric, metric);
        }
    }

    #[test]
    fn point_ids_are_unique() {
        let mut ids: Vec<i64> = POINTS.iter().map(|p| p.point_id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), POINTS.len());
    }

    #[test]
    fn every_default_config_point_maps() {
        for point_id in crate::config::default_points() {
            assert!(for_point(point_id).is_some(), "{point_id} has no metric");
        }
    }

    #[test]
    fn lookups_agree() {
        let soc = for_point(83252).unwrap();
        assert_eq!(soc.metric, Metric::SocPct);
        assert_eq!(soc.scale, 100.0);
        assert_eq!(for_metric(Metric::PvW).point_id, 83033);
        assert_eq!(for_point(1), None);
    }
}
