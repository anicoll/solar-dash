//! HTTP client for the Amber Electric API. Read-only: it only sends GETs.
//!
//! Every call needs `Authorization: Bearer <token>`. The token is kept out of
//! `Debug` output and error messages.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use reqwest::header::{ACCEPT, AUTHORIZATION, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;

use super::models::{PriceInterval, Site, Usage};
use crate::config::Amber;

/// How much of an unexpected reply to quote in an error message.
const SNIPPET_LEN: usize = 200;

/// A parsed reply plus the raw body it came from. The probe can save the raw
/// text to use as a test fixture later.
#[derive(Debug)]
pub struct Reply<T> {
    pub items: Vec<T>,
    pub raw: String,
}

pub struct Client {
    http: reqwest::Client,
    /// Without a trailing slash, so `{base_url}/sites` is the full URL.
    base_url: String,
    // Not `Debug`-derived on purpose: the token must stay out of logs.
    token: String,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Client {
    pub fn new(config: &Amber) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("solar-dash/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not create HTTP client")?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            token: config.token.clone(),
        })
    }

    /// `GET /sites`: every site on the account, closed ones included.
    pub async fn sites(&self) -> Result<Reply<Site>> {
        self.get("/sites", &[]).await
    }

    /// `GET /sites/{id}/prices/current`: the `previous` intervals before now,
    /// the current one, and the next `next` forecast ones, for every channel.
    pub async fn current_prices(
        &self,
        site_id: &str,
        previous: u32,
        next: u32,
    ) -> Result<Reply<PriceInterval>> {
        let path = format!("/sites/{site_id}/prices/current");
        self.get(
            &path,
            &[
                ("previous", previous.to_string()),
                ("next", next.to_string()),
            ],
        )
        .await
    }

    /// `GET /sites/{id}/usage`: metered usage from `start` to `end`, both
    /// inclusive. Amber allows at most 7 days between them.
    pub async fn usage(
        &self,
        site_id: &str,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Reply<Usage>> {
        let path = format!("/sites/{site_id}/usage");
        self.get(
            &path,
            &[
                ("startDate", start.format("%Y-%m-%d").to_string()),
                ("endDate", end.format("%Y-%m-%d").to_string()),
            ],
        )
        .await
    }

    /// Sends a GET, checks the status, and parses the body as a JSON list.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Reply<T>> {
        let mut url = Url::parse(&format!("{}{path}", self.base_url))
            .with_context(|| format!("bad Amber URL for {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let response = self
            .http
            .get(url)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .send()
            .await
            .with_context(|| format!("request to Amber {path} failed"))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let raw = response
            .text()
            .await
            .with_context(|| format!("could not read the reply from Amber {path}"))?;
        check_status(status, retry_after.as_deref(), &raw, path)?;
        let items = serde_json::from_str(&raw).with_context(|| {
            format!(
                "Amber {path} did not return the expected JSON: {}",
                snippet(&raw)
            )
        })?;
        Ok(Reply { items, raw })
    }
}

/// Turns a non-success status into an error with a message that says what to
/// do about it.
fn check_status(
    status: StatusCode,
    retry_after: Option<&str>,
    body: &str,
    path: &str,
) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status {
        StatusCode::UNAUTHORIZED => anyhow!(
            "Amber rejected the token (HTTP 401). Check `token` in [amber]; \
             create a new one at https://app.amber.com.au/developers"
        ),
        StatusCode::NOT_FOUND => anyhow!(
            "Amber says {path} was not found (HTTP 404): the site id is unknown, \
             or the site is closed and Amber no longer serves it"
        ),
        StatusCode::TOO_MANY_REQUESTS => match retry_after {
            Some(wait) => anyhow!("Amber rate limit hit (HTTP 429); retry after {wait}"),
            None => anyhow!("Amber rate limit hit (HTTP 429); try again in a few minutes"),
        },
        _ => anyhow!("Amber {path} failed: HTTP {status}: {}", snippet(body)),
    })
}

/// The first `SNIPPET_LEN` characters of a body, for error messages.
fn snippet(body: &str) -> String {
    let text: String = body.chars().take(SNIPPET_LEN).collect();
    if text.is_empty() {
        "(empty body)".to_string()
    } else {
        text
    }
}

/// Checks the base URL is an http(s) URL; used by the config check.
pub fn check_base_url(base_url: &str) -> Result<()> {
    let url =
        Url::parse(base_url).with_context(|| format!("base_url {base_url:?} is not a URL"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("base_url must start with https:// (or http:// for a test server)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode as AxumStatus};
    use axum::routing::get;

    #[test]
    fn status_errors_are_clear() {
        let err = |status, retry| {
            check_status(status, retry, "body", "/sites")
                .unwrap_err()
                .to_string()
        };
        assert!(err(StatusCode::UNAUTHORIZED, None).contains("token"));
        assert!(err(StatusCode::NOT_FOUND, None).contains("closed"));
        assert!(err(StatusCode::TOO_MANY_REQUESTS, Some("30")).contains("retry after 30"));
        assert!(err(StatusCode::TOO_MANY_REQUESTS, None).contains("rate limit"));
        assert!(err(StatusCode::BAD_GATEWAY, None).contains("502"));
        assert!(check_status(StatusCode::OK, None, "", "/sites").is_ok());
    }

    /// A tiny local server standing in for Amber, so the real client code
    /// (headers, URLs, status handling) runs without the internet.
    async fn fake_amber() -> String {
        let app = Router::new()
            .route(
                "/v1/sites",
                get(|headers: HeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer secret-token")
                    {
                        (AxumStatus::OK, include_str!("testdata/sites.json"))
                    } else {
                        (AxumStatus::UNAUTHORIZED, "{}")
                    }
                }),
            )
            .route(
                "/v1/sites/s1/prices/current",
                get(|query: axum::extract::RawQuery| async move {
                    // Echo that the query reached us by failing when it is wrong.
                    match query.0.as_deref() {
                        Some("previous=12&next=12") => {
                            (AxumStatus::OK, include_str!("testdata/prices_current.json"))
                        }
                        _ => (AxumStatus::BAD_REQUEST, "bad query"),
                    }
                }),
            )
            .route(
                "/v1/sites/s1/usage",
                get(|query: axum::extract::RawQuery| async move {
                    match query.0.as_deref() {
                        Some("startDate=2021-05-01&endDate=2021-05-07") => {
                            (AxumStatus::OK, include_str!("testdata/usage.json"))
                        }
                        _ => (AxumStatus::BAD_REQUEST, "bad query"),
                    }
                }),
            )
            .route(
                "/v1/sites/busy/usage",
                get(|| async {
                    (
                        AxumStatus::TOO_MANY_REQUESTS,
                        [("retry-after", "42")],
                        "slow down",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1/")
    }

    fn config(base_url: String, token: &str) -> Amber {
        Amber {
            token: token.to_string(),
            site_id: None,
            base_url,
        }
    }

    #[tokio::test]
    async fn client_talks_to_the_api() {
        let base = fake_amber().await;
        let client = Client::new(&config(base.clone(), "secret-token")).unwrap();

        let sites = client.sites().await.unwrap();
        assert_eq!(sites.items.len(), 2);
        assert!(sites.raw.contains("SA Power Networks"));

        let prices = client.current_prices("s1", 12, 12).await.unwrap();
        assert_eq!(prices.items.len(), 3);

        let day = |d| NaiveDate::from_ymd_opt(2021, 5, d).unwrap();
        let usage = client.usage("s1", day(1), day(7)).await.unwrap();
        assert_eq!(usage.items.len(), 2);

        let err = client.usage("busy", day(1), day(7)).await.unwrap_err();
        assert!(err.to_string().contains("retry after 42"), "{err}");

        // A wrong token gives the 401 message, and never shows the token.
        let bad = Client::new(&config(base, "wrong")).unwrap();
        let err = format!("{:#}", bad.sites().await.unwrap_err());
        assert!(err.contains("401"), "{err}");
        assert!(!err.contains("wrong"));
        assert!(!format!("{bad:?}").contains("wrong"));
    }
}
