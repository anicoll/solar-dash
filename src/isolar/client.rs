//! A minimal iSolarCloud OpenAPI (V1, non-OAuth) client.
//!
//! The pure pieces ([`build_body`], [`parse_response`]) are separate
//! functions so they can be unit tested without any network.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info};

use super::crypto::{self, Envelope};
use crate::config::IsolarCloud;

/// A power plant ("power station") from the plant list.
#[derive(Debug, Deserialize)]
pub struct Plant {
    pub ps_id: i64,
    pub ps_name: String,
}

pub struct Client {
    http: reqwest::Client,
    config: IsolarCloud,
    token: Option<String>,
}

impl Client {
    pub fn new(config: IsolarCloud) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("solar-dash/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not create HTTP client")?;
        Ok(Self {
            http,
            config,
            token: None,
        })
    }

    /// Sends one API call and returns its `result_data`.
    /// Also used by the `api` subcommand for exploring endpoints.
    pub async fn call(&self, path: &str, params: Value) -> Result<Value> {
        let cfg = &self.config;
        let url = format!("{}{}", cfg.gateway.trim_end_matches('/'), path);
        let body = build_body(&cfg.appkey, params, &crypto::nonce(), &now_millis()?);
        let json = body.to_string();

        let mut request = self
            .http
            .post(&url)
            .header("x-access-key", &cfg.access_key)
            .header("sys_code", "901")
            .header("Content-Type", "application/json");
        if let Some(token) = &self.token {
            request = request.header("token", token);
        }

        // With encryption on, a fresh envelope protects this one request.
        let envelope = if cfg.encrypt {
            Some(Envelope::new(&cfg.rsa_public_key)?)
        } else {
            None
        };
        request = match &envelope {
            Some(env) => request
                .header("x-random-secret-key", &env.encrypted_key)
                .body(env.seal(&json)),
            None => request.body(json),
        };

        let response = request
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;
        let status = response.status();
        let text = response.text().await.context("could not read response")?;
        debug!(%status, %path, len = text.len(), "got response");

        parse_response(&text, envelope.as_ref())
            .with_context(|| format!("{path} failed (HTTP {status})"))
    }

    /// Logs in with the configured username and password and stores the token.
    pub async fn login(&mut self) -> Result<()> {
        let params = json!({
            "login_type": "1",
            "user_account": self.config.username,
            "user_password": self.config.password,
        });
        let data = self.call("/openapi/login", params).await?;
        let token = token_from_login(&data)?;
        info!(token_len = token.len(), "logged in");
        self.token = Some(token);
        Ok(())
    }

    /// Lists the plants on the account. Call [`Client::login`] first.
    pub async fn plants(&self) -> Result<Vec<Plant>> {
        let params = json!({ "curPage": 1, "size": 10 });
        let data = self.call("/openapi/getPowerStationList", params).await?;
        // The exact shape is unconfirmed, so keep the raw data in the debug log.
        debug!(%data, "plant list result_data");
        parse_plants(&data)
    }
}

fn now_millis() -> Result<String> {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(since_epoch.as_millis().to_string())
}

/// Builds the JSON request body: the common fields plus the call's own params.
fn build_body(appkey: &str, params: Value, nonce: &str, timestamp: &str) -> Value {
    let mut body = json!({
        "appkey": appkey,
        "lang": "_en_US",
        "api_key_param": { "nonce": nonce, "timestamp": timestamp },
    });
    // Merge the call's params in (they must be a JSON object).
    if let (Some(body), Some(params)) = (body.as_object_mut(), params.as_object()) {
        for (key, value) in params {
            body.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Turns response text into `result_data`, or an error carrying the API's
/// `result_code` and `result_msg`. The text is plain JSON or, if an envelope
/// is given, may be encrypted hex.
fn parse_response(text: &str, envelope: Option<&Envelope>) -> Result<Value> {
    let text = text.trim();
    // Errors such as a bad key can come back unencrypted even when we
    // encrypted the request, so check for plain JSON first.
    let json_text = match envelope {
        Some(env) if !text.starts_with('{') => {
            env.open(text).context("could not decrypt response")?
        }
        _ => text.to_string(),
    };
    let mut response: Value = serde_json::from_str(&json_text).with_context(|| {
        let preview: String = text.chars().take(80).collect();
        format!("response is not valid JSON (starts with {preview:?})")
    })?;

    // Codes are strings, but be lenient in case one arrives as a number.
    let code = match &response["result_code"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if code == "1" {
        Ok(response["result_data"].take())
    } else {
        // Some errors leave result_msg empty, so fall back to the whole
        // response (error responses carry no token or secrets).
        match response["result_msg"].as_str().filter(|m| !m.is_empty()) {
            Some(msg) => Err(anyhow!(
                "iSolarCloud error: result_code {code:?}, result_msg {msg:?}"
            )),
            None => Err(anyhow!(
                "iSolarCloud error: result_code {code:?}, response {response}"
            )),
        }
    }
}

/// Pulls the token out of the login `result_data`.
fn token_from_login(data: &Value) -> Result<String> {
    if data["login_state"].as_str() != Some("1") {
        let msg = data["msg"].as_str().unwrap_or("no message");
        bail!(
            "login rejected: login_state {}, msg {msg:?}",
            data["login_state"]
        );
    }
    data["token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("login succeeded but result_data has no token"))
}

/// Reads `result_data.pageList` as a list of plants.
fn parse_plants(data: &Value) -> Result<Vec<Plant>> {
    let list = data["pageList"]
        .as_array()
        .ok_or_else(|| anyhow!("plant list has no pageList (see debug log for the raw data)"))?;
    list.iter()
        .map(|item| Plant::deserialize(item).context("could not read a plant"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePublicKey;

    /// A throwaway public key in the format the config expects.
    fn test_public_key() -> String {
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let der = private.to_public_key().to_public_key_der().unwrap();
        base64::engine::general_purpose::URL_SAFE.encode(der.as_bytes())
    }

    #[test]
    fn build_body_merges_params() {
        let body = build_body("key", json!({ "curPage": 1 }), "nonce123", "1700000000000");
        assert_eq!(body["appkey"], "key");
        assert_eq!(body["lang"], "_en_US");
        assert_eq!(body["api_key_param"]["nonce"], "nonce123");
        assert_eq!(body["api_key_param"]["timestamp"], "1700000000000");
        assert_eq!(body["curPage"], 1);
    }

    #[test]
    fn parse_success_returns_result_data() {
        let text = r#"{"result_code":"1","result_msg":"success","result_data":{"a":2}}"#;
        assert_eq!(parse_response(text, None).unwrap()["a"], 2);
    }

    #[test]
    fn parse_error_includes_code_and_message() {
        let text = r#"{"result_code":"E00003","result_msg":"token expired"}"#;
        let msg = parse_response(text, None).unwrap_err().to_string();
        assert!(msg.contains("E00003"));
        assert!(msg.contains("token expired"));
    }

    #[test]
    fn parse_error_without_message_shows_response() {
        let text = r#"{"result_code":"E919","req_serial_num":"abc123"}"#;
        let msg = parse_response(text, None).unwrap_err().to_string();
        assert!(msg.contains("E919"));
        assert!(msg.contains("abc123"));
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(parse_response("<html>bad gateway</html>", None).is_err());
    }

    #[test]
    fn encrypted_round_trip_and_plain_detection() {
        let envelope = Envelope::new(&test_public_key()).unwrap();
        let plain = r#"{"result_code":"1","result_msg":"success","result_data":{"ok":true}}"#;

        // Encrypted response: the server uses the same AES key.
        let sealed = envelope.seal(plain);
        let data = parse_response(&sealed, Some(&envelope)).unwrap();
        assert_eq!(data["ok"], true);

        // Plain JSON is accepted even though we expected encryption.
        let padded = format!("  {plain}\n");
        assert_eq!(
            parse_response(&padded, Some(&envelope)).unwrap()["ok"],
            true
        );

        // Garbage that is neither JSON nor valid ciphertext is an error.
        assert!(parse_response("zzzz", Some(&envelope)).is_err());
    }

    #[test]
    fn login_token_and_failure() {
        let ok = json!({ "login_state": "1", "token": "abc" });
        assert_eq!(token_from_login(&ok).unwrap(), "abc");

        let bad = json!({ "login_state": "0", "msg": "wrong password" });
        let msg = token_from_login(&bad).unwrap_err().to_string();
        assert!(msg.contains("wrong password"));
    }

    #[test]
    fn parses_plant_list() {
        let data = json!({
            "pageList": [{ "ps_id": 1234567, "ps_name": "My House", "extra": 1 }],
            "rowCount": 1
        });
        let plants = parse_plants(&data).unwrap();
        assert_eq!(plants.len(), 1);
        assert_eq!(plants[0].ps_id, 1234567);
        assert_eq!(plants[0].ps_name, "My House");
        assert!(parse_plants(&json!({})).is_err());
    }
}
