//! Configuration loaded from a TOML file.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub isolarcloud: IsolarCloud,
    // `#[serde(default)]` makes the whole section optional: if it is missing,
    // `Default::default()` is used (see the `impl Default` blocks below).
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub database: Database,
}

#[derive(Clone, Deserialize)]
pub struct IsolarCloud {
    #[serde(default = "default_gateway")]
    pub gateway: String,
    pub appkey: String,
    pub access_key: String,
    pub rsa_public_key: String,
    /// Must match the "encrypt" setting of the app in the developer portal.
    #[serde(default = "default_encrypt")]
    pub encrypt: bool,
    pub username: String,
    pub password: String,
    /// Which plant to sync; see config.example.toml.
    pub plant_id: Option<i64>,
    /// Which measuring points to download; see config.example.toml.
    #[serde(default = "default_points")]
    pub points: Vec<i64>,
    /// The plant's time zone (an IANA name). History timestamps are in it.
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

// Deriving Debug would print the secrets into logs, so we write it by hand
// and replace them with a placeholder.
impl fmt::Debug for IsolarCloud {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IsolarCloud")
            .field("gateway", &self.gateway)
            .field("appkey", &self.appkey)
            .field("access_key", &"<redacted>")
            .field("rsa_public_key", &self.rsa_public_key)
            .field("encrypt", &self.encrypt)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("plant_id", &self.plant_id)
            .field("points", &self.points)
            .field("timezone", &self.timezone)
            .finish()
    }
}

#[derive(Deserialize)]
pub struct Server {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Argon2 hash of the dashboard password; make one with `hash-password`.
    pub password_hash: Option<String>,
    /// Base64 of at least 64 random bytes, used to sign login cookies.
    pub session_secret: Option<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            password_hash: None,
            session_secret: None,
        }
    }
}

// Written by hand for the same reason as `IsolarCloud`: keep secrets out of logs.
impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("bind", &self.bind)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "session_secret",
                &self.session_secret.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Deserialize)]
pub struct Database {
    #[serde(default = "default_db_path")]
    pub path: String,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            path: default_db_path(),
        }
    }
}

fn default_gateway() -> String {
    "https://augateway.isolarcloud.com".to_string()
}

fn default_encrypt() -> bool {
    true
}

fn default_points() -> Vec<i64> {
    vec![
        83033, 83067, 83106, 83328, 83326, 83252, 83022, 83102, 83072, 83118, 83322, 83323, 83097,
    ]
}

fn default_timezone() -> String {
    "Australia/Adelaide".to_string()
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_db_path() -> String {
    "data/solar.db".to_string()
}

impl Config {
    /// Read and parse the config file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "could not read config file {} (copy config.example.toml to config.toml and fill it in)",
                path.display()
            )
        })?;
        toml::from_str(&text).with_context(|| format!("invalid config file {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [isolarcloud]
        appkey = "k"
        access_key = "s"
        rsa_public_key = "r"
        username = "u"
        password = "hunter2"
    "#;

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(cfg.isolarcloud.appkey, "YOUR_APPKEY");
    }

    #[test]
    fn defaults_apply_when_sections_omitted() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.isolarcloud.gateway, "https://augateway.isolarcloud.com");
        assert!(cfg.isolarcloud.encrypt);
        assert_eq!(cfg.server.bind, "127.0.0.1:8080");
        assert_eq!(cfg.database.path, "data/solar.db");
        assert_eq!(cfg.isolarcloud.timezone, "Australia/Adelaide");
        assert_eq!(cfg.isolarcloud.points.len(), 13);
        assert!(cfg.isolarcloud.points.contains(&83252));
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("hunter2"));
        let with_server = format!(
            "{MINIMAL}\n[server]\npassword_hash = \"HASHVALUE\"\nsession_secret = \"SECRETVALUE\"\n"
        );
        let cfg: Config = toml::from_str(&with_server).unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("HASHVALUE") && !shown.contains("SECRETVALUE"));
        assert!(!shown.contains("\"s\""));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn missing_file_gives_helpful_error() {
        let err = Config::load(Path::new("does-not-exist.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("config.example.toml"));
    }
}
