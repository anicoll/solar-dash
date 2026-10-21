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
    /// Local Modbus reader. `None` (no `[modbus]` section) means it is off.
    pub modbus: Option<Modbus>,
    /// Electricity prices, used to work out costs and savings. `None` (no
    /// `[tariff]` section) means cost features are off.
    pub tariff: Option<Tariff>,
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

/// How to reach the inverter over Modbus TCP; see config.example.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct Modbus {
    /// IP address or host name of the WiNet-S dongle. No default, so the
    /// section is only valid when you say where the inverter is.
    pub host: String,
    #[serde(default = "default_modbus_port")]
    pub port: u16,
    /// The Modbus unit (slave) id.
    #[serde(default = "default_modbus_unit")]
    pub unit: u8,
    /// Seconds between reads.
    #[serde(default = "default_modbus_interval")]
    pub interval_secs: u64,
}

impl Modbus {
    fn check(&self) -> Result<()> {
        // A zero interval would make the timer loop panic later.
        anyhow::ensure!(
            self.interval_secs >= 1,
            "interval_secs in [modbus] must be at least 1"
        );
        Ok(())
    }
}

/// Electricity prices; see config.example.toml. Rates are in cents (per kWh,
/// except `supply_cents_per_day`). This is the raw file shape: `cost::Tariff`
/// turns it into something quick to look up.
#[derive(Debug, Clone, Deserialize)]
pub struct Tariff {
    /// Fixed daily supply charge.
    pub supply_cents_per_day: f64,
    /// Import price for any time not covered by an `import` window.
    pub default_import_cents: f64,
    /// Import price windows. The first one that matches a time wins.
    #[serde(default)]
    pub import: Vec<PriceWindow>,
    /// Feed-in price for any time not covered by an `export` window.
    #[serde(default)]
    pub default_export_cents: f64,
    /// Feed-in price windows. The first one that matches a time wins.
    #[serde(default)]
    pub export: Vec<PriceWindow>,
    /// Optional daily credit for (almost) no grid import in a window.
    pub zero_import_credit: Option<ZeroImportCredit>,
}

/// A daily time window with a price. `from` is inclusive and `to` exclusive,
/// both local "HH:MM". If `to` is earlier than `from` the window wraps past
/// midnight (for example "21:00" to "06:00").
#[derive(Debug, Clone, Deserialize)]
pub struct PriceWindow {
    pub from: String,
    pub to: String,
    pub cents: f64,
}

/// A credit earned on days when the grid import inside the window stays at or
/// under `max_kwh`.
#[derive(Debug, Clone, Deserialize)]
pub struct ZeroImportCredit {
    pub from: String,
    pub to: String,
    pub max_kwh: f64,
    pub credit_cents: f64,
}

/// Parse a local "HH:MM" time into minutes since midnight (0..=1439).
///
/// We are strict (two digits each side) so a typo like "9:5" is reported
/// instead of being guessed at.
pub fn parse_hhmm(text: &str) -> Result<u32> {
    let bytes = text.as_bytes();
    let digits_ok = bytes.len() == 5
        && bytes[2] == b':'
        && [0, 1, 3, 4].iter().all(|&i| bytes[i].is_ascii_digit());
    anyhow::ensure!(digits_ok, "\"{text}\" is not a time in HH:MM form");
    // The checks above make these slices and parses safe.
    let hours: u32 = text[0..2].parse()?;
    let minutes: u32 = text[3..5].parse()?;
    anyhow::ensure!(
        hours < 24 && minutes < 60,
        "\"{text}\" is not a valid time of day"
    );
    Ok(hours * 60 + minutes)
}

/// Check one window's two times, naming the field `name` in any error.
fn check_window(name: &str, from: &str, to: &str) -> Result<()> {
    let start = parse_hhmm(from).with_context(|| format!("{name}.from in [tariff]"))?;
    let end = parse_hhmm(to).with_context(|| format!("{name}.to in [tariff]"))?;
    // Equal times are ambiguous: empty or the whole day?
    anyhow::ensure!(
        start != end,
        "{name}.from and {name}.to in [tariff] must differ (both are \"{from}\")"
    );
    Ok(())
}

impl Tariff {
    /// Check the section for mistakes the TOML parser cannot catch.
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.supply_cents_per_day >= 0.0,
            "supply_cents_per_day in [tariff] must not be negative"
        );
        for (i, w) in self.import.iter().enumerate() {
            check_window(&format!("import[{i}]"), &w.from, &w.to)?;
        }
        for (i, w) in self.export.iter().enumerate() {
            check_window(&format!("export[{i}]"), &w.from, &w.to)?;
        }
        if let Some(c) = &self.zero_import_credit {
            check_window("zero_import_credit", &c.from, &c.to)?;
        }
        Ok(())
    }
}

fn default_modbus_port() -> u16 {
    502
}

fn default_modbus_unit() -> u8 {
    1
}

fn default_modbus_interval() -> u64 {
    15
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
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        if let Some(modbus) = &config.modbus {
            modbus
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        if let Some(tariff) = &config.tariff {
            tariff
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        Ok(config)
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
    fn modbus_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.modbus.is_none());
    }

    #[test]
    fn modbus_defaults_apply() {
        let cfg: Config =
            toml::from_str(&format!("{MINIMAL}\n[modbus]\nhost = \"10.0.0.5\"\n")).unwrap();
        let modbus = cfg.modbus.unwrap();
        assert_eq!(modbus.host, "10.0.0.5");
        assert_eq!(modbus.port, 502);
        assert_eq!(modbus.unit, 1);
        assert_eq!(modbus.interval_secs, 15);
    }

    #[test]
    fn modbus_section_needs_a_host() {
        let text = format!("{MINIMAL}\n[modbus]\nport = 502\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn modbus_interval_must_be_positive() {
        let text = format!("{MINIMAL}\n[modbus]\nhost = \"h\"\ninterval_secs = 0\n");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.modbus.unwrap().check().is_err());
    }

    const TARIFF: &str = r#"
        [tariff]
        supply_cents_per_day = 173.8
        default_import_cents = 44.0
        import = [{ from = "11:00", to = "14:00", cents = 0.0 }]
        export = [{ from = "18:00", to = "21:00", cents = 10.0 }]
        [tariff.zero_import_credit]
        from = "18:00"
        to = "21:00"
        max_kwh = 0.1
        credit_cents = 100.0
    "#;

    /// Parse MINIMAL plus a tariff section and run the tariff check.
    fn check_tariff(section: &str) -> Result<()> {
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{section}")).unwrap();
        cfg.tariff.unwrap().check()
    }

    #[test]
    fn tariff_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.tariff.is_none());
    }

    #[test]
    fn tariff_parses_and_defaults_apply() {
        check_tariff(TARIFF).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{TARIFF}")).unwrap();
        let tariff = cfg.tariff.unwrap();
        assert_eq!(tariff.import[0].to, "14:00");
        assert_eq!(tariff.zero_import_credit.unwrap().credit_cents, 100.0);
        // Only the two required prices are needed.
        let minimal = "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 30.0\n";
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{minimal}")).unwrap();
        let tariff = cfg.tariff.unwrap();
        assert!(tariff.import.is_empty() && tariff.export.is_empty());
        assert_eq!(tariff.default_export_cents, 0.0);
        assert!(tariff.zero_import_credit.is_none());
    }

    #[test]
    fn parses_times_of_day() {
        assert_eq!(parse_hhmm("00:00").unwrap(), 0);
        assert_eq!(parse_hhmm("21:30").unwrap(), 21 * 60 + 30);
        assert_eq!(parse_hhmm("23:59").unwrap(), 1439);
        for bad in ["", "9:05", "24:00", "12:60", "12-30", "ab:cd", "12:300"] {
            assert!(parse_hhmm(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn tariff_errors_name_the_field() {
        let base = "supply_cents_per_day = 1.0\ndefault_import_cents = 1.0\n";
        let cases = [
            (
                "supply_cents_per_day = -1.0\ndefault_import_cents = 1.0\n",
                "supply_cents_per_day",
            ),
            (
                &format!("{base}import = [{{ from = \"25:00\", to = \"26:00\", cents = 1.0 }}]"),
                "import[0].from",
            ),
            (
                &format!(
                    "{base}export = [{{ from = \"09:00\", to = \"10:00\", cents = 1.0 }}, {{ from = \"09:00\", to = \"nope\", cents = 1.0 }}]"
                ),
                "export[1].to",
            ),
            (
                &format!("{base}import = [{{ from = \"09:00\", to = \"09:00\", cents = 1.0 }}]"),
                "import[0].from and import[0].to",
            ),
            (
                &format!(
                    "{base}zero_import_credit = {{ from = \"09:00\", to = \"09:00\", max_kwh = 0.1, credit_cents = 1.0 }}"
                ),
                "zero_import_credit.from",
            ),
        ];
        for (body, field) in cases {
            let err = check_tariff(&format!("[tariff]\n{body}")).unwrap_err();
            assert!(format!("{err:#}").contains(field), "{field}: {err:#}");
        }
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
