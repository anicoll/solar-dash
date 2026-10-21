//! Configuration loaded from a TOML file.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

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
    /// TeslaMate's MQTT broker, for the car's charge state. `None` (no
    /// `[teslamate]` section) means it is off.
    pub teslamate: Option<Teslamate>,
    /// Electricity prices, used to work out costs and savings. `None` (no
    /// `[tariff]` section) means cost features are off.
    pub tariff: Option<Tariff>,
    /// Plans that applied before `[tariff]`, oldest first, as repeated
    /// `[[previous_tariff]]` tables. Needs `[tariff]` as well.
    #[serde(default)]
    pub previous_tariff: Vec<PreviousTariff>,
    /// What the system cost, for the payback estimate. Optional.
    #[serde(default)]
    pub system: System,
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
    /// First day ("YYYY-MM-DD") of history `serve` fills in by itself.
    pub history_from: Option<String>,
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
            .field("history_from", &self.history_from)
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
    /// Required by `serve`.
    pub session_secret: Option<String>,
    /// Whether the session cookie gets the `Secure` flag. Keep this true
    /// (the default) behind https; set false only for plain-http LAN testing.
    #[serde(default = "default_cookie_secure")]
    pub cookie_secure: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            password_hash: None,
            session_secret: None,
            cookie_secure: default_cookie_secure(),
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
            .field("cookie_secure", &self.cookie_secure)
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

/// How to reach TeslaMate's MQTT broker; see config.example.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct Teslamate {
    /// IP address or host name of the MQTT broker. No default, so the
    /// section is only valid when you say where it is.
    pub host: String,
    #[serde(default = "default_teslamate_port")]
    pub port: u16,
    /// Which car to follow (TeslaMate numbers its cars from 1).
    #[serde(default = "default_teslamate_car_id")]
    pub car_id: u32,
    /// The start of every topic name. TeslaMate uses "teslamate" unless its
    /// `MQTT_NAMESPACE` setting says otherwise.
    #[serde(default = "default_teslamate_topic_prefix")]
    pub topic_prefix: String,
    /// Broker login. Give both or neither.
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Teslamate {
    fn check(&self) -> Result<()> {
        // A login needs both parts; one alone is almost certainly a typo.
        anyhow::ensure!(
            self.username.is_some() == self.password.is_some(),
            "[teslamate] needs both username and password, or neither"
        );
        Ok(())
    }
}

/// Electricity prices; see config.example.toml. Rates are in cents (per kWh,
/// except `supply_cents_per_day`). This is the raw file shape: `cost::Tariff`
/// turns it into something quick to look up.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PriceWindow {
    pub from: String,
    pub to: String,
    pub cents: f64,
}

/// A credit earned on days when the grid import inside the window stays at or
/// under `max_kwh`.
#[derive(Debug, Clone, Deserialize, Serialize)]
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
fn check_window(section: &str, name: &str, from: &str, to: &str) -> Result<()> {
    let start = parse_hhmm(from).with_context(|| format!("{name}.from in {section}"))?;
    let end = parse_hhmm(to).with_context(|| format!("{name}.to in {section}"))?;
    // Equal times are ambiguous: empty or the whole day?
    anyhow::ensure!(
        start != end,
        "{name}.from and {name}.to in {section} must differ (both are \"{from}\")"
    );
    Ok(())
}

impl Tariff {
    /// Check the section for mistakes the TOML parser cannot catch. `section`
    /// names it in errors ("[tariff]" or "[[previous_tariff]] #1").
    fn check(&self, section: &str) -> Result<()> {
        anyhow::ensure!(
            self.supply_cents_per_day >= 0.0,
            "supply_cents_per_day in {section} must not be negative"
        );
        for (i, w) in self.import.iter().enumerate() {
            check_window(section, &format!("import[{i}]"), &w.from, &w.to)?;
        }
        for (i, w) in self.export.iter().enumerate() {
            check_window(section, &format!("export[{i}]"), &w.from, &w.to)?;
        }
        if let Some(c) = &self.zero_import_credit {
            check_window(section, "zero_import_credit", &c.from, &c.to)?;
        }
        Ok(())
    }
}

/// A plan that was in effect before `[tariff]`: the same fields, plus the last
/// day it applied. Written as repeated `[[previous_tariff]]` tables.
#[derive(Debug, Clone, Deserialize)]
pub struct PreviousTariff {
    /// Last local day ("YYYY-MM-DD", inclusive) this plan applied.
    pub until: String,
    /// A label for the config file; not used for anything.
    #[allow(dead_code)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub tariff: Tariff,
}

/// Check the previous plans: each one like `[tariff]`, `until` dates that
/// parse and strictly increase down the file, and a current `[tariff]`.
fn check_previous_tariffs(previous: &[PreviousTariff], has_current: bool) -> Result<()> {
    anyhow::ensure!(
        previous.is_empty() || has_current,
        "[[previous_tariff]] needs a [tariff] section for the plan since the last `until`"
    );
    let mut last: Option<NaiveDate> = None;
    for (i, p) in previous.iter().enumerate() {
        let section = format!("[[previous_tariff]] #{}", i + 1);
        let until = parse_date(&p.until).with_context(|| format!("until in {section}"))?;
        anyhow::ensure!(
            last.is_none_or(|prev| until > prev),
            "until in {section} must be later than the one before it"
        );
        last = Some(until);
        p.tariff.check(&section)?;
    }
    Ok(())
}

/// The solar system's purchases, for `/api/payback`; see config.example.toml.
/// Written as repeated `[[system.purchase]]` tables.
#[derive(Debug, Deserialize)]
pub struct System {
    #[serde(default)]
    pub purchase: Vec<Purchase>,
    /// Assumed yearly rise in electricity prices, in percent, which makes the
    /// savings grow in the payback projection.
    #[serde(default = "default_price_rise_percent")]
    pub price_rise_percent: f64,
}

impl Default for System {
    fn default() -> Self {
        System {
            purchase: Vec::new(),
            price_rise_percent: default_price_rise_percent(),
        }
    }
}

fn default_price_rise_percent() -> f64 {
    3.0
}

/// One thing bought for the system: the panels and inverter, a second battery.
#[derive(Debug, Clone, Deserialize)]
pub struct Purchase {
    pub name: String,
    /// Local date it was paid for, "YYYY-MM-DD".
    pub date: String,
    pub cost_dollars: f64,
}

/// Parse a "YYYY-MM-DD" date. Strict, like [`parse_hhmm`].
pub fn parse_date(text: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .with_context(|| format!("\"{text}\" is not a date in YYYY-MM-DD form"))
}

impl System {
    /// Check the purchases, naming the field in any error.
    fn check(&self) -> Result<()> {
        // The range check is false for NaN too.
        anyhow::ensure!(
            (0.0..=20.0).contains(&self.price_rise_percent),
            "price_rise_percent in [system] must be between 0 and 20"
        );
        for (i, p) in self.purchase.iter().enumerate() {
            anyhow::ensure!(
                !p.name.trim().is_empty(),
                "purchase[{i}].name in [system] must not be empty"
            );
            parse_date(&p.date).with_context(|| format!("purchase[{i}].date in [system]"))?;
            // `>= 0.0` is false for NaN as well, so that is rejected too.
            anyhow::ensure!(
                p.cost_dollars >= 0.0 && p.cost_dollars.is_finite(),
                "purchase[{i}].cost_dollars in [system] must be zero or more"
            );
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

fn default_teslamate_port() -> u16 {
    1883
}

fn default_teslamate_car_id() -> u32 {
    1
}

fn default_teslamate_topic_prefix() -> String {
    "teslamate".to_string()
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

fn default_cookie_secure() -> bool {
    true
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
        if let Some(teslamate) = &config.teslamate {
            teslamate
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        if let Some(tariff) = &config.tariff {
            tariff
                .check("[tariff]")
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        check_previous_tariffs(&config.previous_tariff, config.tariff.is_some())
            .with_context(|| format!("invalid config file {}", path.display()))?;
        config
            .system
            .check()
            .with_context(|| format!("invalid config file {}", path.display()))?;
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

    #[test]
    fn teslamate_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.teslamate.is_none());
    }

    #[test]
    fn teslamate_defaults_apply() {
        let cfg: Config =
            toml::from_str(&format!("{MINIMAL}\n[teslamate]\nhost = \"10.0.0.9\"\n")).unwrap();
        let teslamate = cfg.teslamate.unwrap();
        assert_eq!(teslamate.host, "10.0.0.9");
        assert_eq!(teslamate.port, 1883);
        assert_eq!(teslamate.car_id, 1);
        assert_eq!(teslamate.topic_prefix, "teslamate");
        assert_eq!(teslamate.username, None);
        assert_eq!(teslamate.password, None);
        assert!(teslamate.check().is_ok());
    }

    #[test]
    fn teslamate_section_needs_a_host() {
        let text = format!("{MINIMAL}\n[teslamate]\nport = 1883\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn teslamate_username_and_password_go_together() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        assert!(
            parse("username = \"u\"\npassword = \"p\"\n")
                .check()
                .is_ok()
        );
        assert!(parse("username = \"u\"\n").check().is_err());
        assert!(parse("password = \"p\"\n").check().is_err());
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
        cfg.tariff.unwrap().check("[tariff]")
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

    /// Parse MINIMAL plus these sections and run the previous-plan checks.
    fn check_previous(sections: &str) -> Result<()> {
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{sections}")).unwrap();
        check_previous_tariffs(&cfg.previous_tariff, cfg.tariff.is_some())
    }

    const CURRENT: &str = "[tariff]\nsupply_cents_per_day = 100.0\ndefault_import_cents = 30.0\n";

    fn previous(until: &str) -> String {
        format!(
            "[[previous_tariff]]\nuntil = \"{until}\"\nname = \"Old\"\n\
             supply_cents_per_day = 90.0\ndefault_import_cents = 20.0\n\
             import = [{{ from = \"11:00\", to = \"14:00\", cents = 0.0 }}]\n"
        )
    }

    #[test]
    fn previous_tariffs_parse_with_the_same_fields() {
        let text = format!(
            "{CURRENT}{}{}",
            previous("2025-06-30"),
            previous("2026-01-31")
        );
        check_previous(&text).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{text}")).unwrap();
        assert_eq!(cfg.previous_tariff.len(), 2);
        let first = &cfg.previous_tariff[0];
        assert_eq!(first.until, "2025-06-30");
        assert_eq!(first.tariff.default_import_cents, 20.0);
        assert_eq!(first.tariff.import[0].to, "14:00");
        // A credit table after an entry belongs to that entry.
        let with_credit = format!(
            "{CURRENT}{}[previous_tariff.zero_import_credit]\nfrom = \"18:00\"\n\
             to = \"21:00\"\nmax_kwh = 0.1\ncredit_cents = 50.0\n",
            previous("2025-06-30")
        );
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{with_credit}")).unwrap();
        let credit = cfg.previous_tariff[0].tariff.zero_import_credit.as_ref();
        assert_eq!(credit.unwrap().credit_cents, 50.0);
        // None is fine too.
        check_previous(CURRENT).unwrap();
    }

    #[test]
    fn previous_tariff_errors_name_the_plan() {
        let bad_window = previous("2025-06-30").replace("14:00", "25:00");
        let cases = [
            (
                format!("{CURRENT}{}", previous("2025-13-01")),
                "until in [[previous_tariff]] #1",
            ),
            (
                format!(
                    "{CURRENT}{}{}",
                    previous("2025-06-30"),
                    previous("2025-06-30")
                ),
                "#2 must be later",
            ),
            (
                format!(
                    "{CURRENT}{}{}",
                    previous("2025-06-30"),
                    previous("2025-01-01")
                ),
                "#2 must be later",
            ),
            (
                format!("{CURRENT}{bad_window}"),
                "import[0].to in [[previous_tariff]] #1",
            ),
            (previous("2025-06-30"), "needs a [tariff]"),
        ];
        for (text, expected) in cases {
            let err = check_previous(&text).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{expected}: {err:#}");
        }
    }

    const PURCHASES: &str = r#"
        [[system.purchase]]
        name = "Solar"
        date = "2023-02-21"
        cost_dollars = 19490.00
        [[system.purchase]]
        name = "Battery 2"
        date = "2025-10-01"
        cost_dollars = 7690.20
    "#;

    /// Parse MINIMAL plus a `[system]` section and run its check.
    fn check_system(section: &str) -> Result<()> {
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{section}")).unwrap();
        cfg.system.check()
    }

    #[test]
    fn purchases_are_optional_and_parse() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.system.purchase.is_empty());
        check_system(PURCHASES).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{PURCHASES}")).unwrap();
        assert_eq!(cfg.system.purchase.len(), 2);
        assert_eq!(cfg.system.purchase[1].cost_dollars, 7690.20);
    }

    #[test]
    fn price_rise_defaults_and_is_checked() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.system.price_rise_percent, 3.0);
        check_system("[system]\nprice_rise_percent = 0").unwrap();
        check_system("[system]\nprice_rise_percent = 20").unwrap();
        for bad in ["-1", "20.5", "nan"] {
            let err = check_system(&format!("[system]\nprice_rise_percent = {bad}")).unwrap_err();
            assert!(format!("{err:#}").contains("price_rise_percent"), "{err:#}");
        }
    }

    #[test]
    fn purchase_errors_name_the_field() {
        let one = |body: &str| format!("[[system.purchase]]\n{body}");
        let cases = [
            (
                one("name = \" \"\ndate = \"2023-02-21\"\ncost_dollars = 1.0"),
                "purchase[0].name",
            ),
            (
                one("name = \"a\"\ndate = \"21/02/2023\"\ncost_dollars = 1.0"),
                "purchase[0].date",
            ),
            (
                one("name = \"a\"\ndate = \"2023-02-30\"\ncost_dollars = 1.0"),
                "purchase[0].date",
            ),
            (
                one("name = \"a\"\ndate = \"2023-02-21\"\ncost_dollars = -5.0"),
                "purchase[0].cost_dollars",
            ),
            (
                format!(
                    "{}\n{}",
                    one("name = \"a\"\ndate = \"2023-02-21\"\ncost_dollars = 1.0"),
                    one("name = \"b\"\ndate = \"2023-02-21\"\ncost_dollars = nan")
                ),
                "purchase[1].cost_dollars",
            ),
        ];
        for (section, field) in cases {
            let err = check_system(&section).unwrap_err();
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
