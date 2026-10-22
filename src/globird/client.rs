//! HTTP client for the GloBird customer portal: login plus generic JSON calls.
//!
//! The portal sets session cookies at login, so the client keeps a cookie jar.
//! Every JSON reply looks like `{"data": ..., "message": ..., "success": bool}`;
//! [`Client::get_json`] and [`Client::post_json`] check that and hand back just
//! the `data` part.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use reqwest::cookie::Jar;
use reqwest::header::{ACCEPT, ORIGIN, REFERER, SET_COOKIE};
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use rsa::{BigUint, Oaep, RsaPublicKey};
use serde_json::{Value, json};
use sha2::Sha256;
use tracing::{debug, info};

use crate::config::Globird;

/// The portal's address. It is also sent as the `Origin` and `Referer`.
const BASE_URL: &str = "https://myaccount.globirdenergy.com.au";

/// How much of an unexpected reply to quote in an error message.
const SNIPPET_LEN: usize = 200;

/// URL-safe base64 that accepts input with or without `=` padding (JWK values
/// are normally unpadded).
const LENIENT_URL_SAFE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The portal wants a captcha before it will log in. A separate error type
/// (instead of a plain message) so the sync can tell it apart from a network
/// error: retrying does not help, only logging in once in a browser does. Find
/// it in an `anyhow::Error` with `err.downcast_ref::<CaptchaRequired>()`.
#[derive(Debug)]
pub struct CaptchaRequired;

impl fmt::Display for CaptchaRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GloBird wants a captcha before it will log in. Log in once in a browser at \
             {BASE_URL} and try again later"
        )
    }
}

impl std::error::Error for CaptchaRequired {}

/// A client for the portal. Call [`Client::login`] before anything else.
pub struct Client {
    http: reqwest::Client,
    /// The same jar the client uses, kept so we can add cookies by hand.
    jar: Arc<Jar>,
    base: Url,
    // Not `Debug`-derived on purpose: the password must stay out of logs.
    email: String,
    password: String,
}

impl Client {
    pub fn new(config: Globird) -> Result<Self> {
        let jar = Arc::new(Jar::default());
        let http = reqwest::Client::builder()
            .cookie_provider(Arc::clone(&jar))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("solar-dash/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not create HTTP client")?;
        Ok(Self {
            http,
            jar,
            base: Url::parse(BASE_URL).context("bad portal base URL")?,
            email: config.email,
            password: config.password,
        })
    }

    /// Logs in and keeps the session cookies in the jar.
    pub async fn login(&self) -> Result<()> {
        self.fetch_affinity_cookies().await;

        let jwk = self.get_raw("/api/account/publicjwk").await?;
        let key = parse_jwk(&jwk).context("could not read the portal's public key")?;
        let encrypted = encrypt_password(&self.password, &key)?;

        let url = self.url("/api/account/login", &[])?;
        let request = self.request(Method::POST, url).json(&json!({
            "emailAddress": self.email,
            "password": encrypted,
            "rememberMe": false,
        }));
        let (status, body) = self.send(request, "/api/account/login").await?;
        check_login_reply(status, &body)?;
        info!("logged in to the GloBird portal");
        Ok(())
    }

    /// Sends a GET and returns the `data` part of the reply.
    pub async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let request = self.request(Method::GET, self.url(path, query)?);
        let (status, body) = self.send(request, path).await?;
        parse_envelope(status, &body).with_context(|| format!("{path} failed"))
    }

    /// Sends a POST with a JSON body and returns the `data` part of the reply.
    pub async fn post_json(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: &Value,
    ) -> Result<Value> {
        let request = self
            .request(Method::POST, self.url(path, query)?)
            .json(body);
        let (status, text) = self.send(request, path).await?;
        parse_envelope(status, &text).with_context(|| format!("{path} failed"))
    }

    /// Visits the homepage and copies its Azure sticky-routing cookies into
    /// the jar for the portal's own address. Best effort: any failure is only
    /// logged, because login may work without them.
    async fn fetch_affinity_cookies(&self) {
        let request = self.request(Method::GET, self.base.clone());
        // The portal sets these cookies for another domain
        // (globirdcustomerportalprod.azurewebsites.net), so the jar would not
        // send them to us. We read the Set-Cookie headers ourselves.
        //
        // Redirects are followed (the default policy), so cookies set on an
        // intermediate redirect response are not seen; only the final
        // response's headers are. If that turns out to matter, turn
        // redirects off for this one request.
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                debug!("homepage request failed (ignored): {err}");
                return;
            }
        };
        let headers = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok());
        let cookies = affinity_cookies(headers);
        // Log how many, never the values.
        debug!(count = cookies.len(), "adding ARRAffinity cookies");
        for (name, value) in cookies {
            self.jar
                .add_cookie_str(&format!("{name}={value}"), &self.base);
        }
    }

    /// GETs a path and returns the whole reply body as JSON, not unwrapped.
    /// For the public key, which is a bare object.
    async fn get_raw(&self, path: &str) -> Result<Value> {
        let request = self.request(Method::GET, self.url(path, &[])?);
        let (status, body) = self.send(request, path).await?;
        if !status.is_success() {
            bail!("{path} failed: HTTP {status}: {}", snippet(&body));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("{path} did not return JSON: {}", snippet(&body)))
    }

    /// A request with the headers the portal expects on every call.
    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.http
            .request(method, url)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(ORIGIN, BASE_URL)
            .header(REFERER, format!("{BASE_URL}/"))
    }

    /// The full URL for a path and query parameters.
    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let mut url = self
            .base
            .join(path)
            .with_context(|| format!("bad path {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Sends a request and reads the whole body. `path` is only for messages.
    async fn send(&self, request: RequestBuilder, path: &str) -> Result<(StatusCode, String)> {
        let response = request
            .send()
            .await
            .with_context(|| format!("request to {path} failed"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("could not read the reply from {path}"))?;
        debug!(%status, %path, len = body.len(), "got response");
        Ok((status, body))
    }
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

/// Picks the `name=value` pairs of the Azure `ARRAffinity*` cookies out of
/// `Set-Cookie` header values. Attributes such as `Domain=` are dropped.
fn affinity_cookies<'a>(
    set_cookie_headers: impl Iterator<Item = &'a str>,
) -> Vec<(String, String)> {
    set_cookie_headers
        .filter_map(|header| {
            // The pair comes before the first `;`.
            let pair = header.split(';').next()?;
            let (name, value) = pair.split_once('=')?;
            let name = name.trim();
            name.to_ascii_lowercase()
                .contains("arraffinity")
                .then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Builds an RSA public key from the portal's JWK (`n` and `e`, base64url).
fn parse_jwk(jwk: &Value) -> Result<RsaPublicKey> {
    // Be forgiving: accept the key wrapped in `data` too.
    let jwk = if jwk.get("n").is_none() && jwk.get("data").is_some() {
        &jwk["data"]
    } else {
        jwk
    };
    let part = |name: &str| -> Result<BigUint> {
        let text = jwk
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the key has no \"{name}\" field"))?;
        let bytes = LENIENT_URL_SAFE
            .decode(text.trim())
            .with_context(|| format!("\"{name}\" in the key is not valid base64url"))?;
        Ok(BigUint::from_bytes_be(&bytes))
    };
    RsaPublicKey::new(part("n")?, part("e")?).context("the portal's public key is not valid")
}

/// Encrypts the password with RSA-OAEP (SHA-256 for both the hash and MGF1)
/// and returns it as padded standard base64.
fn encrypt_password(password: &str, key: &RsaPublicKey) -> Result<String> {
    let mut rng = rsa::rand_core::OsRng;
    let encrypted = key
        .encrypt(&mut rng, Oaep::new::<Sha256>(), password.as_bytes())
        .context("could not encrypt the password")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

/// The portal's `message` field, if it has a text one.
fn message_of(reply: &Value) -> Option<&str> {
    reply
        .get("message")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
}

/// Checks the reply to the login call. Unlike other calls, `success: false`
/// is not enough to bail early: a captcha demand also needs its own message.
fn check_login_reply(status: StatusCode, body: &str) -> Result<()> {
    let Ok(reply) = serde_json::from_str::<Value>(body) else {
        bail!("login failed: HTTP {status}: {}", snippet(body));
    };
    let data = &reply["data"];
    let flag = |name: &str| data.get(name).and_then(Value::as_bool) == Some(true);
    if flag("requireHCaptcha") || flag("requireRetryCaptCha") {
        return Err(CaptchaRequired.into());
    }
    let succeeded = reply.get("success").and_then(Value::as_bool) != Some(false)
        && data.get("isLoginSucceeded").and_then(Value::as_bool) != Some(false)
        && status.is_success();
    if !succeeded {
        let message = message_of(&reply)
            .or_else(|| data.get("message").and_then(Value::as_str))
            .unwrap_or("no message from the portal");
        bail!("login failed (HTTP {status}): {message}");
    }
    Ok(())
}

/// Turns an HTTP status and body into the reply's `data` value.
fn parse_envelope(status: StatusCode, body: &str) -> Result<Value> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        bail!("not logged in or the session expired (HTTP {status})");
    }
    if !status.is_success() {
        bail!("HTTP {status}: {}", snippet(body));
    }
    let mut reply: Value = serde_json::from_str(body)
        .with_context(|| format!("reply is not JSON: {}", snippet(body)))?;
    if reply.get("success").and_then(Value::as_bool) == Some(false) {
        bail!(
            "the portal reported failure: {}",
            message_of(&reply).unwrap_or("no message")
        );
    }
    // `take` moves the value out and leaves `null` behind.
    Ok(reply
        .get_mut("data")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::RsaPrivateKey;
    use rsa::traits::PublicKeyParts;

    fn b64url(n: &BigUint) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(n.to_bytes_be())
    }

    #[test]
    fn jwk_parses_with_and_without_padding() {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
        let public = private.to_public_key();
        let n = b64url(public.n());
        let e = b64url(public.e());
        // Pad to a multiple of 4, as a padded encoder would.
        let padded = |text: &str| format!("{text}{}", "=".repeat((4 - text.len() % 4) % 4));
        for (n, e) in [(n.clone(), e.clone()), (padded(&n), padded(&e))] {
            let key = parse_jwk(&json!({"n": n, "e": e, "kty": "RSA"})).unwrap();
            assert_eq!(key, public);
        }
        // Wrapped in `data` is accepted too.
        let wrapped = parse_jwk(&json!({"data": {"n": n, "e": e}})).unwrap();
        assert_eq!(wrapped, public);
    }

    #[test]
    fn jwk_errors_are_clear() {
        let err = parse_jwk(&json!({"e": "AQAB"})).unwrap_err();
        assert!(format!("{err:#}").contains("\"n\""));
        assert!(parse_jwk(&json!({"n": "!!!", "e": "AQAB"})).is_err());
    }

    #[test]
    fn password_round_trips_through_oaep_sha256() {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
        let encrypted = encrypt_password("pässword 123", &private.to_public_key()).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&encrypted)
            .unwrap();
        let plain = private.decrypt(Oaep::new::<Sha256>(), &bytes).unwrap();
        assert_eq!(plain, "pässword 123".as_bytes());
    }

    #[test]
    fn finds_affinity_cookies_only() {
        let headers = [
            "ARRAffinity=abc123; path=/; HttpOnly; Domain=globirdcustomerportalprod.azurewebsites.net",
            "ARRAffinitySameSite=def456; path=/; Secure; SameSite=None",
            "other=1; path=/",
            "garbage without equals",
        ];
        let cookies = affinity_cookies(headers.into_iter());
        assert_eq!(
            cookies,
            vec![
                ("ARRAffinity".to_string(), "abc123".to_string()),
                ("ARRAffinitySameSite".to_string(), "def456".to_string()),
            ]
        );
    }

    #[test]
    fn affinity_name_match_ignores_case() {
        let cookies = affinity_cookies(["arraffinity=x"].into_iter());
        assert_eq!(cookies.len(), 1);
    }

    #[test]
    fn envelope_returns_data() {
        let body = r#"{"data": {"a": 1}, "message": null, "success": true}"#;
        assert_eq!(
            parse_envelope(StatusCode::OK, body).unwrap(),
            json!({"a": 1})
        );
        // No `data` at all gives null.
        assert_eq!(
            parse_envelope(StatusCode::OK, r#"{"success": true}"#).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn envelope_errors() {
        let expired = parse_envelope(StatusCode::UNAUTHORIZED, "").unwrap_err();
        assert!(expired.to_string().contains("expired"));
        assert!(parse_envelope(StatusCode::FORBIDDEN, "").is_err());
        let server = parse_envelope(StatusCode::INTERNAL_SERVER_ERROR, "boom").unwrap_err();
        assert!(server.to_string().contains("500"));
        let failed = parse_envelope(
            StatusCode::OK,
            r#"{"data": null, "message": "No such account", "success": false}"#,
        )
        .unwrap_err();
        assert!(failed.to_string().contains("No such account"));
        assert!(parse_envelope(StatusCode::OK, "<html>").is_err());
    }

    #[test]
    fn login_reply_cases() {
        let ok = r#"{"data": {"isLoginSucceeded": true}, "success": true}"#;
        assert!(check_login_reply(StatusCode::OK, ok).is_ok());

        let captcha =
            r#"{"data": {"requireHCaptcha": true, "isLoginSucceeded": false}, "success": false}"#;
        let err = check_login_reply(StatusCode::OK, captcha).unwrap_err();
        assert!(err.to_string().contains("captcha"));
        // The sync finds this type to tell a captcha from a network error.
        assert!(err.downcast_ref::<CaptchaRequired>().is_some());
        let retry = r#"{"data": {"requireRetryCaptCha": true}, "success": true}"#;
        assert!(check_login_reply(StatusCode::OK, retry).is_err());

        let bad = r#"{"data": {"isLoginSucceeded": false}, "message": "Wrong password", "success": false}"#;
        let err = check_login_reply(StatusCode::OK, bad).unwrap_err();
        assert!(err.to_string().contains("login failed"));
        assert!(err.to_string().contains("Wrong password"));

        let not_ok = r#"{"data": null, "message": "Locked", "success": true}"#;
        assert!(check_login_reply(StatusCode::FORBIDDEN, not_ok).is_err());
        assert!(check_login_reply(StatusCode::OK, "not json").is_err());
    }
}
