//! The JSON the Amber API returns, as serde types.
//!
//! The field names follow Amber's OpenAPI spec (https://api.amber.com.au/v1).
//! Unknown fields are ignored (serde's default), so Amber adding a field does
//! not break us. Fields the spec does not mark as required are `Option`.
//! Enums have an `Unknown` case so a new value from Amber does not make the
//! whole reply fail to parse.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Whether a site is still supplied by Amber.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SiteStatus {
    Pending,
    Active,
    Closed,
    #[serde(other)]
    Unknown,
}

/// Which meter channel a price or usage row is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChannelType {
    General,
    ControlledLoad,
    FeedIn,
    #[serde(other)]
    Unknown,
}

impl ChannelType {
    /// The name as Amber spells it, for tables.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelType::General => "general",
            ChannelType::ControlledLoad => "controlledLoad",
            ChannelType::FeedIn => "feedIn",
            ChannelType::Unknown => "unknown",
        }
    }
}

/// One meter channel of a site.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Channel {
    pub identifier: String,
    #[serde(rename = "type")]
    pub kind: ChannelType,
    pub tariff: Option<String>,
}

/// A site (a meter, identified by its NMI) on the account.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub id: String,
    pub nmi: String,
    #[serde(default)]
    pub channels: Vec<Channel>,
    pub network: Option<String>,
    pub status: SiteStatus,
    /// `YYYY-MM-DD`. Missing for some sites (Amber asks you to contact them).
    pub active_from: Option<String>,
    /// `YYYY-MM-DD`. Only present for closed sites.
    pub closed_on: Option<String>,
    /// Billing interval in minutes: 5 or 30.
    pub interval_length: Option<u32>,
}

/// Which kind of price interval this is. Amber tags each one with `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum IntervalKind {
    /// A past interval with its final price.
    ActualInterval,
    /// The interval happening now; the price may still change.
    CurrentInterval,
    /// A future interval, with a predicted price.
    ForecastInterval,
    #[serde(other)]
    Unknown,
}

impl IntervalKind {
    pub fn short_name(self) -> &'static str {
        match self {
            IntervalKind::ActualInterval => "actual",
            IntervalKind::CurrentInterval => "current",
            IntervalKind::ForecastInterval => "forecast",
            IntervalKind::Unknown => "unknown",
        }
    }
}

/// Amber's low/predicted/high band for a forecast, in cents per kWh.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdvancedPrice {
    pub low: f64,
    pub predicted: f64,
    pub high: f64,
}

/// A spot price range, in cents per kWh.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PriceRange {
    pub min: f64,
    pub max: f64,
}

/// One price interval from `/sites/{id}/prices/current`. All three kinds
/// (actual, current, forecast) share one struct; the few fields that only some
/// kinds have are `Option`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceInterval {
    #[serde(rename = "type")]
    pub kind: IntervalKind,
    pub channel_type: ChannelType,
    /// Cents per kWh including GST, as Amber returns it. For `feedIn` a
    /// NEGATIVE price means the customer is paid (confirmed against the Amber
    /// app), and a positive one means they pay to export.
    pub per_kwh: f64,
    /// NEM spot price in cents per kWh including GST.
    pub spot_per_kwh: f64,
    /// Minutes.
    pub duration: u32,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    /// NEM date (not local) the interval belongs to, `YYYY-MM-DD`.
    pub date: Option<String>,
    pub renewables: Option<f64>,
    pub descriptor: Option<String>,
    pub spike_status: Option<String>,
    /// Only on current intervals: true while the price can still change.
    pub estimate: Option<bool>,
    pub range: Option<PriceRange>,
    pub advanced_price: Option<AdvancedPrice>,
}

/// One usage row from `/sites/{id}/usage`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub channel_type: ChannelType,
    pub channel_identifier: String,
    /// Energy in kWh. Amber says generated energy is negative.
    pub kwh: f64,
    /// Total cost for the interval, in cents including GST.
    pub cost: f64,
    pub per_kwh: f64,
    pub spot_per_kwh: Option<f64>,
    /// `estimated` or `billable`.
    pub quality: Option<String>,
    /// Minutes.
    pub duration: u32,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sites() {
        let sites: Vec<Site> = serde_json::from_str(include_str!("testdata/sites.json")).unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].id, "01F5A5CRKMZ5BCX9P1S4V990AM");
        assert_eq!(sites[0].status, SiteStatus::Active);
        assert_eq!(sites[0].channels.len(), 2);
        assert_eq!(sites[0].channels[1].kind, ChannelType::FeedIn);
        assert_eq!(sites[0].active_from.as_deref(), Some("2022-01-01"));
        assert!(sites[0].closed_on.is_none());
        assert_eq!(sites[1].status, SiteStatus::Closed);
        assert_eq!(sites[1].closed_on.as_deref(), Some("2022-05-01"));
        assert_eq!(sites[1].interval_length, Some(5));
    }

    #[test]
    fn parses_prices_of_every_kind() {
        let prices: Vec<PriceInterval> =
            serde_json::from_str(include_str!("testdata/prices_current.json")).unwrap();
        assert_eq!(prices.len(), 3);

        assert_eq!(prices[0].kind, IntervalKind::ActualInterval);
        assert_eq!(prices[0].channel_type, ChannelType::General);
        assert_eq!(prices[0].per_kwh, 24.33);
        assert_eq!(prices[0].duration, 30);
        assert_eq!(
            prices[0].start_time.to_rfc3339(),
            "2021-05-05T02:00:01+00:00"
        );
        assert!(prices[0].estimate.is_none());

        // A negative feed-in price and the current-interval extras.
        assert_eq!(prices[1].kind, IntervalKind::CurrentInterval);
        assert_eq!(prices[1].channel_type, ChannelType::FeedIn);
        assert_eq!(prices[1].per_kwh, -4.2);
        assert_eq!(prices[1].estimate, Some(true));
        assert_eq!(prices[1].range.as_ref().unwrap().max, 3.0);
        assert_eq!(prices[1].advanced_price.as_ref().unwrap().predicted, 3.0);

        // `null` extras and a missing `tariffInformation` are fine.
        assert_eq!(prices[2].kind, IntervalKind::ForecastInterval);
        assert_eq!(prices[2].channel_type, ChannelType::ControlledLoad);
        assert!(prices[2].range.is_none());
        assert!(prices[2].advanced_price.is_none());
    }

    #[test]
    fn parses_usage() {
        let usage: Vec<Usage> = serde_json::from_str(include_str!("testdata/usage.json")).unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].channel_identifier, "E1");
        assert_eq!(usage[0].kwh, 0.42);
        assert_eq!(usage[0].quality.as_deref(), Some("billable"));
        assert_eq!(usage[1].channel_type, ChannelType::FeedIn);
        assert_eq!(usage[1].kwh, -1.3);
        assert_eq!(usage[1].cost, -6.5);
    }

    #[test]
    fn unknown_enum_values_do_not_fail_parsing() {
        let site: Site = serde_json::from_str(
            r#"{"id":"x","nmi":"1","status":"suspended","channels":[{"identifier":"E1","type":"battery"}]}"#,
        )
        .unwrap();
        assert_eq!(site.status, SiteStatus::Unknown);
        assert_eq!(site.channels[0].kind, ChannelType::Unknown);
    }
}
