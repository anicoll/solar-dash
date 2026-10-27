//! Configuration loaded from a TOML file.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveTime};
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
    /// Login for the GloBird customer portal, used by `globird-probe` and by
    /// `serve`'s daily GloBird sync. `None` (no `[globird]` section) means it
    /// is off.
    pub globird: Option<Globird>,
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
    /// Past bills whose real totals replace our estimate. Optional.
    pub bills: Option<Bills>,
}

/// The `[bills]` section.
#[derive(Debug, Clone, Deserialize)]
pub struct Bills {
    /// CSV of past bills; see docs/how-to-run.md.
    pub file: String,
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
    /// A secret for scripts: `Authorization: Bearer <token>` is accepted on
    /// `/api/` routes as well as the login cookie. Off when unset.
    pub api_token: Option<String>,
}

/// The shortest `api_token` allowed (`openssl rand -hex 32` makes 64).
const MIN_API_TOKEN_LEN: usize = 32;

impl Server {
    fn check(&self) -> Result<()> {
        if let Some(token) = &self.api_token {
            // Not echoed in the error: it is a secret.
            anyhow::ensure!(
                token.len() >= MIN_API_TOKEN_LEN,
                "api_token in [server] must be at least {MIN_API_TOKEN_LEN} characters \
                 (make one with `openssl rand -hex 32`)"
            );
        }
        Ok(())
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            password_hash: None,
            session_secret: None,
            cookie_secure: default_cookie_secure(),
            api_token: None,
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
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
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
    /// What petrol costs, in cents per litre. No default: without it the
    /// car-versus-petrol comparison is off.
    pub petrol_cents_per_litre: Option<f64>,
    /// How much petrol the comparison car uses per 100 km.
    #[serde(default = "default_petrol_litres_per_100km")]
    pub petrol_litres_per_100km: f64,
    /// Petrol prices that changed over time, for the per-car history. Written
    /// as repeated `[[teslamate.petrol_price]]` tables.
    #[serde(default)]
    pub petrol_price: Vec<PetrolPrice>,
    /// A CSV file of petrol prices that changed over time, which replaces the
    /// `[[teslamate.petrol_price]]` entries (set one or the other). It can be
    /// edited by hand or through `/api/petrol-prices`; see `petrol_prices.rs`.
    /// A missing file is treated as empty and created on the first change.
    pub petrol_prices_file: Option<String>,
    /// A flat price per kWh for charging away from home. Without it, away
    /// sessions are left uncosted.
    pub away_cents_per_kwh: Option<f64>,
    /// TeslaMate's Postgres database, such as
    /// `postgres://solar_dash:secret@192.168.10.84:5432/teslamate`. Without
    /// it the history sync is off. It holds a password, so it is never logged.
    pub database_url: Option<String>,
    /// The petrol car each TeslaMate model is compared with, by TeslaMate's
    /// model string (such as "3" or "Y"), for the "EV versus petrol" view of
    /// `/api/ev/cars`. A model without an entry has no comparison car. Write it
    /// before the `[[teslamate.petrol_price]]` tables, as TOML needs.
    #[serde(default)]
    pub compare_cars: BTreeMap<String, String>,
    /// The grade of petrol the price and the comparison are for.
    #[serde(default = "default_petrol_grade")]
    pub petrol_grade: String,
}

impl Teslamate {
    fn check(&self) -> Result<()> {
        // A login needs both parts; one alone is almost certainly a typo.
        anyhow::ensure!(
            self.username.is_some() == self.password.is_some(),
            "[teslamate] needs both username and password, or neither"
        );
        // `> 0.0` is false for NaN too.
        anyhow::ensure!(
            self.petrol_litres_per_100km > 0.0,
            "petrol_litres_per_100km in [teslamate] must be more than 0"
        );
        for (name, value) in [
            ("petrol_cents_per_litre", self.petrol_cents_per_litre),
            ("away_cents_per_kwh", self.away_cents_per_kwh),
        ] {
            anyhow::ensure!(
                value.is_none_or(|v| v > 0.0),
                "{name} in [teslamate] must be more than 0"
            );
        }
        if let Some(file) = &self.petrol_prices_file {
            anyhow::ensure!(
                !file.trim().is_empty(),
                "petrol_prices_file in [teslamate] must not be empty"
            );
            anyhow::ensure!(
                self.petrol_price.is_empty(),
                "[teslamate] sets both petrol_prices_file and [[teslamate.petrol_price]]; \
                 use one or the other (the file's entries replace the tables)"
            );
        }
        anyhow::ensure!(
            !self.petrol_grade.trim().is_empty(),
            "petrol_grade in [teslamate] must not be empty"
        );
        for (model, car) in &self.compare_cars {
            anyhow::ensure!(
                !model.trim().is_empty() && !car.trim().is_empty(),
                "compare_cars in [teslamate] needs a model and a car name in every entry"
            );
        }
        let mut last: Option<NaiveDate> = None;
        for (i, p) in self.petrol_price.iter().enumerate() {
            let section = format!("[[teslamate.petrol_price]] #{}", i + 1);
            let from = parse_date(&p.from).with_context(|| format!("from in {section}"))?;
            anyhow::ensure!(
                last.is_none_or(|prev| from > prev),
                "from in {section} must be later than the one before it"
            );
            last = Some(from);
            anyhow::ensure!(
                p.cents_per_litre > 0.0,
                "cents_per_litre in {section} must be more than 0"
            );
        }
        if let Some(url) = &self.database_url {
            // Not echoed in the error: it holds a password.
            anyhow::ensure!(
                url.starts_with("postgres://") || url.starts_with("postgresql://"),
                "database_url in [teslamate] must start with postgres:// or postgresql://"
            );
        }
        Ok(())
    }
}

/// What petrol cost from one day on; see `[[teslamate.petrol_price]]` in
/// config.example.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct PetrolPrice {
    /// First local day ("YYYY-MM-DD") this price applies.
    pub from: String,
    pub cents_per_litre: f64,
}

/// Login for the GloBird customer portal; see config.example.toml.
#[derive(Clone, Deserialize)]
pub struct Globird {
    pub email: String,
    pub password: String,
    /// Local time of day ("HH:MM") for the daily sync of GloBird's data.
    #[serde(default = "default_globird_sync_time")]
    pub sync_time: String,
}

impl Globird {
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.email.trim().is_empty(),
            "email in [globird] must not be empty"
        );
        anyhow::ensure!(
            !self.password.is_empty(),
            "password in [globird] must not be empty"
        );
        self.sync_time_of_day()?;
        Ok(())
    }

    /// `sync_time` as a time of day.
    pub fn sync_time_of_day(&self) -> Result<NaiveTime> {
        let minutes = parse_hhmm(&self.sync_time).context("sync_time in [globird]")?;
        // `parse_hhmm` guarantees less than 24 hours, so this always succeeds.
        NaiveTime::from_hms_opt(minutes / 60, minutes % 60, 0)
            .context("sync_time in [globird] is not a valid time of day")
    }
}

// Written by hand for the same reason as `IsolarCloud`: keep the password out of logs.
impl fmt::Debug for Globird {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Globird")
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .field("sync_time", &self.sync_time)
            .finish()
    }
}

/// Electricity prices; see config.example.toml. Rates are in cents (per kWh,
/// except `supply_cents_per_day`). This is the raw file shape: `cost::Tariff`
/// turns it into something quick to look up.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tariff {
    /// A label for the plan, shown on the payback timeline and in
    /// `/api/ev/cars`. It is not part of the rates, so it is left out of the
    /// stored key and naming a plan does not make stored costs stale. (In a
    /// `[[previous_tariff]]` the table's own `name` is the one that counts.)
    #[serde(default, skip_serializing)]
    pub name: Option<String>,
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
    /// Optional daily tiered feed-in: the first kWh exported each day earn a
    /// different rate. Left out of the stored key when unset, so plans without
    /// it keep their stored costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_tier: Option<ExportTier>,
}

/// A feed-in rate for the first `first_kwh_per_day` kWh exported in each local
/// day (in time order from midnight), instead of the time-of-day rate.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportTier {
    pub first_kwh_per_day: f64,
    pub cents: f64,
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
    pub(crate) fn check(&self, section: &str) -> Result<()> {
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
        if let Some(t) = &self.export_tier {
            anyhow::ensure!(
                t.first_kwh_per_day.is_finite() && t.first_kwh_per_day > 0.0,
                "export_tier.first_kwh_per_day in {section} must be greater than 0"
            );
            anyhow::ensure!(
                t.cents.is_finite() && t.cents >= 0.0,
                "export_tier.cents in {section} must not be negative"
            );
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
    /// A label for the plan, shown on the payback timeline and in
    /// `/api/ev/cars`; not used in any maths.
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

pub(crate) fn default_price_rise_percent() -> f64 {
    3.0
}

/// One thing bought for the system: the panels and inverter, a second battery.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Purchase {
    pub name: String,
    /// Local date it was paid for, "YYYY-MM-DD".
    pub date: String,
    pub cost_dollars: f64,
    /// Capacity of the battery this purchase added, in kWh. Optional; with it
    /// `/api/payback` estimates the savings this battery added.
    #[serde(default)]
    pub battery_kwh: Option<f64>,
}

impl Purchase {
    /// Check one purchase; `label` names it in errors ("purchase[0]").
    pub(crate) fn check(&self, label: &str) -> Result<()> {
        anyhow::ensure!(
            !self.name.trim().is_empty(),
            "{label}.name in [system] must not be empty"
        );
        parse_date(&self.date).with_context(|| format!("{label}.date in [system]"))?;
        // `>= 0.0` is false for NaN as well, so that is rejected too.
        anyhow::ensure!(
            self.cost_dollars >= 0.0 && self.cost_dollars.is_finite(),
            "{label}.cost_dollars in [system] must be zero or more"
        );
        if let Some(kwh) = self.battery_kwh {
            anyhow::ensure!(
                kwh.is_finite() && kwh > 0.0,
                "{label}.battery_kwh in [system] must be greater than 0"
            );
        }
        Ok(())
    }
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
            p.check(&format!("purchase[{i}]"))?;
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

pub(crate) fn default_petrol_grade() -> String {
    "Unleaded 91".to_string()
}

pub(crate) fn default_petrol_litres_per_100km() -> f64 {
    7.5
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

fn default_globird_sync_time() -> String {
    "09:00".to_string()
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
        config
            .server
            .check()
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
        if let Some(globird) = &config.globird {
            globird
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
    fn globird_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.globird.is_none());
    }

    #[test]
    fn globird_section_needs_email_and_password() {
        let ok = format!("{MINIMAL}\n[globird]\nemail = \"a@b.c\"\npassword = \"pw\"\n");
        let globird = toml::from_str::<Config>(&ok).unwrap().globird.unwrap();
        assert_eq!(globird.email, "a@b.c");
        assert!(globird.check().is_ok());

        let no_password = format!("{MINIMAL}\n[globird]\nemail = \"a@b.c\"\n");
        assert!(toml::from_str::<Config>(&no_password).is_err());

        let empty = format!("{MINIMAL}\n[globird]\nemail = \"\"\npassword = \"pw\"\n");
        let globird = toml::from_str::<Config>(&empty).unwrap().globird.unwrap();
        assert!(globird.check().is_err());
    }

    #[test]
    fn globird_sync_time_defaults_and_is_checked() {
        let parse = |extra: &str| -> Globird {
            let text =
                format!("{MINIMAL}\n[globird]\nemail = \"a@b.c\"\npassword = \"pw\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().globird.unwrap()
        };
        let default = parse("");
        assert_eq!(default.sync_time, "09:00");
        assert!(default.check().is_ok());
        let evening = parse("sync_time = \"18:30\"");
        assert!(evening.check().is_ok());
        assert_eq!(
            evening.sync_time_of_day().unwrap(),
            NaiveTime::from_hms_opt(18, 30, 0).unwrap()
        );
        for bad in ["9:00", "25:00", "09:60", "noon", ""] {
            let globird = parse(&format!("sync_time = \"{bad}\""));
            let err = globird.check().unwrap_err();
            assert!(format!("{err:#}").contains("sync_time"), "{bad}: {err:#}");
        }
    }

    #[test]
    fn globird_password_is_not_in_debug_output() {
        let globird = Globird {
            email: "a@b.c".into(),
            password: "hunter2".into(),
            sync_time: "09:00".into(),
        };
        let text = format!("{globird:?}");
        assert!(!text.contains("hunter2"));
        assert!(text.contains("<redacted>"));
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
        assert_eq!(teslamate.petrol_cents_per_litre, None);
        assert_eq!(teslamate.petrol_litres_per_100km, 7.5);
        assert_eq!(teslamate.away_cents_per_kwh, None);
        assert!(teslamate.check().is_ok());
    }

    #[test]
    fn teslamate_section_needs_a_host() {
        let text = format!("{MINIMAL}\n[teslamate]\nport = 1883\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn teslamate_comparison_cars_and_petrol_grade() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        let plain = parse("");
        assert!(plain.compare_cars.is_empty());
        assert_eq!(plain.petrol_grade, "Unleaded 91");
        assert!(plain.check().is_ok());

        let set = parse(
            "compare_cars = { \"3\" = \"Hyundai i30 Sedan N\", \"Y\" = \"Toyota RAV4 GX 2WD\" }\n\
             petrol_grade = \"Premium 95\"\n",
        );
        assert_eq!(set.compare_cars["Y"], "Toyota RAV4 GX 2WD");
        assert_eq!(set.petrol_grade, "Premium 95");
        assert!(set.check().is_ok());

        assert!(parse("petrol_grade = \" \"\n").check().is_err());
        assert!(parse("compare_cars = { \"Y\" = \"\" }\n").check().is_err());
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

    #[test]
    fn teslamate_petrol_and_away_prices_must_be_positive() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        let good = parse(
            "petrol_cents_per_litre = 195.0\npetrol_litres_per_100km = 6.5\naway_cents_per_kwh = 65.0\n",
        );
        assert!(good.check().is_ok());
        assert_eq!(good.petrol_cents_per_litre, Some(195.0));
        assert_eq!(good.petrol_litres_per_100km, 6.5);
        assert_eq!(good.away_cents_per_kwh, Some(65.0));
        for bad in [
            "petrol_cents_per_litre = 0.0",
            "petrol_cents_per_litre = -190.0",
            "petrol_litres_per_100km = 0.0",
            "petrol_litres_per_100km = nan",
            "away_cents_per_kwh = -1.0",
        ] {
            let err = parse(bad).check().unwrap_err();
            assert!(format!("{err:#}").contains("more than 0"), "{bad}: {err:#}");
        }
    }

    #[test]
    fn teslamate_petrol_prices_are_dated_and_increasing() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        let entry = |from: &str, cents: &str| {
            format!("[[teslamate.petrol_price]]\nfrom = \"{from}\"\ncents_per_litre = {cents}\n")
        };
        assert!(parse("").petrol_price.is_empty());
        let good = parse(&format!(
            "{}{}",
            entry("2025-01-01", "185.0"),
            entry("2026-03-01", "199.0")
        ));
        assert!(good.check().is_ok());
        assert_eq!(good.petrol_price.len(), 2);
        assert_eq!(good.petrol_price[1].cents_per_litre, 199.0);
        for (bad, expect) in [
            (
                entry("March", "199.0"),
                "from in [[teslamate.petrol_price]] #1",
            ),
            (entry("2026-03-01", "0.0"), "more than 0"),
            (entry("2026-03-01", "-5.0"), "more than 0"),
            (
                format!(
                    "{}{}",
                    entry("2026-03-01", "199.0"),
                    entry("2026-03-01", "200.0")
                ),
                "#2 must be later",
            ),
            (
                format!(
                    "{}{}",
                    entry("2026-03-01", "199.0"),
                    entry("2025-03-01", "200.0")
                ),
                "#2 must be later",
            ),
        ] {
            let err = parse(&bad).check().unwrap_err();
            assert!(format!("{err:#}").contains(expect), "{bad}: {err:#}");
        }
    }

    #[test]
    fn server_api_token_must_be_long_enough() {
        let parse = |extra: &str| -> Server {
            let text = format!("{MINIMAL}\n[server]\n{extra}");
            toml::from_str::<Config>(&text).unwrap().server
        };
        assert_eq!(parse("").api_token, None);
        assert!(parse("").check().is_ok());
        let good = parse(&format!("api_token = \"{}\"\n", "a".repeat(32)));
        assert!(good.check().is_ok());
        for bad in ["", "short", &"a".repeat(31)] {
            let err = parse(&format!("api_token = \"{bad}\"\n"))
                .check()
                .unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("at least 32"), "{message}");
            assert!(bad.is_empty() || !message.contains(bad), "{message}");
        }
        // The Debug output hides it.
        assert!(!format!("{good:?}").contains("aaaa"));
    }

    #[test]
    fn teslamate_petrol_prices_file_excludes_the_tables() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        assert_eq!(parse("").petrol_prices_file, None);
        let good = parse("petrol_prices_file = \"/tmp/petrol.csv\"\n");
        assert!(good.check().is_ok());
        assert_eq!(good.petrol_prices_file.as_deref(), Some("/tmp/petrol.csv"));

        let both = parse(
            "petrol_prices_file = \"/tmp/petrol.csv\"\n\
             [[teslamate.petrol_price]]\nfrom = \"2026-03-01\"\ncents_per_litre = 199.0\n",
        );
        let err = both.check().unwrap_err();
        assert!(
            format!("{err:#}").contains("both petrol_prices_file"),
            "{err:#}"
        );
        let err = parse("petrol_prices_file = \" \"\n").check().unwrap_err();
        assert!(format!("{err:#}").contains("must not be empty"), "{err:#}");
    }

    #[test]
    fn teslamate_database_url_must_be_postgres() {
        let parse = |extra: &str| -> Teslamate {
            let text = format!("{MINIMAL}\n[teslamate]\nhost = \"h\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().teslamate.unwrap()
        };
        assert_eq!(parse("").database_url, None);
        for good in [
            "postgres://u:p@h:5432/teslamate",
            "postgresql://u@h/teslamate",
        ] {
            let teslamate = parse(&format!("database_url = \"{good}\""));
            assert!(teslamate.check().is_ok(), "{good}");
            assert_eq!(teslamate.database_url.as_deref(), Some(good));
        }
        let err = parse("database_url = \"mysql://u:secret@h/db\"")
            .check()
            .unwrap_err();
        assert!(format!("{err:#}").contains("postgres://"), "{err:#}");
        assert!(!format!("{err:#}").contains("secret"));
    }

    const TARIFF: &str = r#"
        [tariff]
        supply_cents_per_day = 150.0
        default_import_cents = 40.0
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

    #[test]
    fn export_tier_parses_and_is_checked() {
        let ok = format!("{CURRENT}[tariff.export_tier]\nfirst_kwh_per_day = 14.0\ncents = 15.0\n");
        check_tariff(&ok).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{ok}")).unwrap();
        let tier = cfg.tariff.unwrap().export_tier.unwrap();
        assert_eq!((tier.first_kwh_per_day, tier.cents), (14.0, 15.0));
        // Zero cents is a valid (if odd) rate.
        check_tariff(&ok.replace("15.0", "0.0")).unwrap();
        check_tariff(CURRENT).unwrap();
        let cases = [
            (
                "first_kwh_per_day = 0.0\ncents = 15.0",
                "export_tier.first_kwh_per_day in [tariff]",
            ),
            (
                "first_kwh_per_day = -1.0\ncents = 15.0",
                "export_tier.first_kwh_per_day in [tariff]",
            ),
            (
                "first_kwh_per_day = 14.0\ncents = -1.0",
                "export_tier.cents in [tariff]",
            ),
        ];
        for (body, expected) in cases {
            let text = format!("{CURRENT}[tariff.export_tier]\n{body}\n");
            let err = check_tariff(&text).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{expected}: {err:#}");
        }
    }

    #[test]
    fn plans_may_be_named() {
        let text = format!("{CURRENT}name = \"Now\"\n{}", previous("2025-06-30"));
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{text}")).unwrap();
        assert_eq!(cfg.tariff.unwrap().name.as_deref(), Some("Now"));
        assert_eq!(cfg.previous_tariff[0].name.as_deref(), Some("Old"));
        // The previous plan's name is its own, not the flattened tariff's.
        assert_eq!(cfg.previous_tariff[0].tariff.name, None);
        let unnamed: Config = toml::from_str(&format!("{MINIMAL}\n{CURRENT}")).unwrap();
        assert_eq!(unnamed.tariff.unwrap().name, None);
    }

    #[test]
    fn export_tier_in_a_previous_tariff() {
        let text = format!(
            "{CURRENT}{}[previous_tariff.export_tier]\nfirst_kwh_per_day = 14.0\ncents = 15.0\n",
            previous("2025-06-30")
        );
        check_previous(&text).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{text}")).unwrap();
        assert!(cfg.tariff.unwrap().export_tier.is_none());
        let tier = cfg.previous_tariff[0].tariff.export_tier.as_ref().unwrap();
        assert_eq!((tier.first_kwh_per_day, tier.cents), (14.0, 15.0));
        let bad = text.replace("14.0", "0.0");
        let err = check_previous(&bad).unwrap_err();
        assert!(
            format!("{err:#}").contains("export_tier.first_kwh_per_day in [[previous_tariff]] #1"),
            "{err:#}"
        );
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
        date = "2024-01-01"
        cost_dollars = 12000.00
        [[system.purchase]]
        name = "Battery 2"
        date = "2025-06-01"
        cost_dollars = 8000.00
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
        assert_eq!(cfg.system.purchase[1].cost_dollars, 8000.00);
    }

    #[test]
    fn battery_kwh_is_optional_and_checked() {
        let with = format!(
            "{PURCHASES}\n[[system.purchase]]\nname = \"c\"\ndate = \"2025-10-01\"\n\
             cost_dollars = 7690.0\nbattery_kwh = 12.8\n"
        );
        check_system(&with).unwrap();
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{with}")).unwrap();
        assert_eq!(cfg.system.purchase[0].battery_kwh, None);
        assert_eq!(cfg.system.purchase[2].battery_kwh, Some(12.8));
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
                one("name = \" \"\ndate = \"2024-01-01\"\ncost_dollars = 1.0"),
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
                one("name = \"a\"\ndate = \"2024-01-01\"\ncost_dollars = -5.0"),
                "purchase[0].cost_dollars",
            ),
            (
                format!(
                    "{}\n{}",
                    one("name = \"a\"\ndate = \"2024-01-01\"\ncost_dollars = 1.0"),
                    one("name = \"b\"\ndate = \"2024-01-01\"\ncost_dollars = nan")
                ),
                "purchase[1].cost_dollars",
            ),
            (
                one("name = \"a\"\ndate = \"2024-01-01\"\ncost_dollars = 1.0\nbattery_kwh = 0"),
                "purchase[0].battery_kwh",
            ),
            (
                one("name = \"a\"\ndate = \"2024-01-01\"\ncost_dollars = 1.0\nbattery_kwh = -2.5"),
                "purchase[0].battery_kwh",
            ),
            (
                one("name = \"a\"\ndate = \"2024-01-01\"\ncost_dollars = 1.0\nbattery_kwh = nan"),
                "purchase[0].battery_kwh",
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
