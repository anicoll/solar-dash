//! HTTP client for the Amber Electric API, plus the types its replies parse into.
//!
//! Authentication is a bearer token. The reply types are deliberately lenient:
//! almost every field is optional, so a shape surprise cannot make the probe
//! fail. Prices and costs are in cents, GST included.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::config::Amber;

const BASE_URL: &str = "https://api.amber.com.au/v1";

/// How much of an unexpected reply to quote in a message.
const SNIPPET_LEN: usize = 200;

/// One property with a meter, from `GET /sites`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    #[serde(default)]
    pub id: String,
    pub nmi: Option<String>,
    pub status: Option<String>,
    pub active_from: Option<String>,
    pub closed_on: Option<String>,
    #[serde(default)]
    pub channels: Vec<Channel>,
}

/// One meter channel of a site.
#[derive(Debug, Deserialize)]
pub struct Channel {
    pub identifier: Option<String>,
    /// "general", "controlledLoad" or "feedIn".
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub tariff: Option<String>,
}

/// One 30-minute usage interval, from `GET /sites/{id}/usage`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// The local date the interval belongs to.
    pub date: Option<String>,
    pub channel_type: Option<String>,
    pub channel_identifier: Option<String>,
    pub kwh: Option<f64>,
    /// Cents, GST included.
    pub cost: Option<f64>,
    /// Cents per kWh, GST included.
    pub per_kwh: Option<f64>,
    /// "estimated" or "billable".
    pub quality: Option<String>,
}

/// A reply with whatever the server sent, even if the status is an error.
pub struct Reply {
    pub status: StatusCode,
    /// Rate limit headers (and `Retry-After`) as (name, value) pairs.
    pub rate_limit: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    /// The first part of the body, for error messages.
    pub fn snippet(&self) -> String {
        snippet(&self.body)
    }
}

/// A client for the API.
pub struct Client {
    http: reqwest::Client,
    base: String,
    // Not `Debug`-derived on purpose: the token must stay out of logs.
    token: String,
}

impl Client {
    pub fn new(config: &Amber) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("solar-dash/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not create HTTP client")?;
        Ok(Self {
            http,
            base: BASE_URL.to_string(),
            token: config.token.clone(),
        })
    }

    /// `GET /sites`.
    pub async fn sites(&self) -> Result<Reply> {
        self.get("/sites", &[]).await
    }

    /// `GET /sites/{id}/usage` for the inclusive date range, at 30 minutes.
    pub async fn usage(&self, site_id: &str, from: NaiveDate, to: NaiveDate) -> Result<Reply> {
        let query = [
            ("startDate", from.to_string()),
            ("endDate", to.to_string()),
            ("resolution", "30".to_string()),
        ];
        self.get(&format!("/sites/{site_id}/usage"), &query).await
    }

    /// Sends a GET. A non-2xx status is not an error here: the probe wants to
    /// see it. Only a failure to get any reply is.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Reply> {
        let url = Url::parse_with_params(&format!("{}{path}", self.base), query)
            .with_context(|| format!("bad URL for {path}"))?;
        // `bearer_auth` marks the header as sensitive, so it is kept out of
        // debug output.
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("request to {path} failed"))?;
        let status = response.status();
        let rate_limit = response
            .headers()
            .iter()
            .filter(|(name, _)| {
                let name = name.as_str();
                name.contains("ratelimit") || name.contains("rate-limit") || name == "retry-after"
            })
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or("<not text>").to_string(),
                )
            })
            .collect();
        let body = response
            .text()
            .await
            .with_context(|| format!("could not read the reply from {path}"))?;
        Ok(Reply {
            status,
            rate_limit,
            body,
        })
    }
}

/// The first `SNIPPET_LEN` characters of a body, on one line.
fn snippet(body: &str) -> String {
    let text: String = body.chars().take(SNIPPET_LEN).collect();
    text.replace('\n', " ")
}

/// Parses a `GET /sites` body.
pub fn parse_sites(body: &str) -> Result<Vec<Site>> {
    serde_json::from_str(body)
        .with_context(|| format!("sites reply is not the expected list: {}", snippet(body)))
}

/// Parses a `GET /sites/{id}/usage` body.
pub fn parse_usage(body: &str) -> Result<Vec<Usage>> {
    serde_json::from_str(body)
        .with_context(|| format!("usage reply is not the expected list: {}", snippet(body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITES_JSON: &str = include_str!("testdata/sites.json");
    const USAGE_JSON: &str = include_str!("testdata/usage.json");

    #[test]
    fn parses_sites_and_ignores_unknown_fields() {
        let sites = parse_sites(SITES_JSON).unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].status.as_deref(), Some("closed"));
        assert_eq!(sites[0].active_from.as_deref(), Some("2024-01-15"));
        assert_eq!(sites[0].closed_on.as_deref(), Some("2025-12-31"));
        assert_eq!(sites[0].channels.len(), 2);
        assert_eq!(sites[0].channels[1].kind.as_deref(), Some("feedIn"));
        // Missing fields become None or empty rather than errors.
        assert!(sites[1].closed_on.is_none());
        assert!(sites[1].channels.is_empty());
    }

    #[test]
    fn parses_usage_with_missing_fields() {
        let rows = parse_usage(USAGE_JSON).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].channel_identifier.as_deref(), Some("E1"));
        assert_eq!(rows[0].kwh, Some(0.5));
        assert_eq!(rows[0].per_kwh, Some(20.0));
        assert_eq!(rows[0].quality.as_deref(), Some("billable"));
        assert_eq!(rows[2].cost, Some(-10.0));
        assert!(rows[3].quality.is_none());
    }

    #[test]
    fn wrong_shapes_are_clear_errors() {
        let err = parse_usage(r#"{"message": "Not authorised"}"#).unwrap_err();
        assert!(format!("{err:#}").contains("Not authorised"));
        assert!(parse_sites("").is_err());
    }
}
