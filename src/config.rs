//! Configuration loaded from a TOML file.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct Config {
    /// The iSolarCloud account. `None` (no `[isolarcloud]` section) means the
    /// cloud sync is off; Modbus, tariffs and the rest still work.
    pub isolarcloud: Option<IsolarCloud>,
    /// Site-wide settings that do not belong to one data source. Optional.
    pub site: Option<Site>,
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
    /// Amber Electric API token, used by `amber-probe`. `None` (no `[amber]`
    /// section) means it is off.
    pub amber: Option<Amber>,
    /// Publishes live readings to Home Assistant over MQTT. `None` (no
    /// `[home_assistant]` section) means nothing is published.
    pub home_assistant: Option<HomeAssistant>,
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
    /// Which source wins when two can answer the same question. Optional;
    /// without it the defaults give the long-standing behaviour.
    #[serde(default)]
    pub sources: Sources,
}

/// The `[site]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// The site's time zone (an IANA name). It decides where a day ends.
    /// Wins over `timezone` in `[isolarcloud]`.
    pub timezone: Option<String>,
}

/// The `[sources]` section; see `hub::rules` for what the settings mean.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// Source ids for a day's energy, best first.
    pub daily_energy: Option<Vec<String>>,
    pub finished_day_coverage: Option<f64>,
    pub partial_day_coverage: Option<f64>,
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
    /// Where the nightly backups go. Default: a `backups` directory next to
    /// the database file.
    pub backup_dir: Option<String>,
    /// How many daily backups to keep; 0 turns backups off.
    #[serde(default = "default_backup_keep")]
    pub backup_keep: usize,
    /// Optional copy of each nightly backup in an S3-compatible bucket such as
    /// Cloudflare R2. Absent: no offsite copy.
    pub offsite: Option<Offsite>,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            backup_dir: None,
            backup_keep: default_backup_keep(),
            offsite: None,
        }
    }
}

/// Where to send each nightly backup, as `[database.offsite]`; see
/// config.example.toml.
#[derive(Clone, Deserialize)]
pub struct Offsite {
    /// Cloudflare account id; the store is then
    /// `https://<account_id>.r2.cloudflarestorage.com`.
    pub account_id: Option<String>,
    /// The store's address instead of `account_id`, such as
    /// `https://s3.example.com` (for another S3-compatible store, or a test).
    pub endpoint: Option<String>,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Prepended to every object name. Default `solar-dash/`.
    #[serde(default = "default_offsite_prefix")]
    pub prefix: String,
    /// How many backups to keep in the bucket (the newest by date in the name).
    #[serde(default = "default_offsite_keep")]
    pub keep: usize,
}

impl Offsite {
    /// The store's address, with no trailing slash.
    pub fn endpoint(&self) -> String {
        match (&self.endpoint, &self.account_id) {
            (Some(endpoint), _) => endpoint.trim_end_matches('/').to_string(),
            (None, Some(id)) => format!("https://{id}.r2.cloudflarestorage.com"),
            (None, None) => String::new(),
        }
    }

    /// The prefix as it is used in object names: no leading slash, and a
    /// trailing slash unless empty.
    pub fn prefix(&self) -> String {
        let trimmed = self.prefix.trim_matches('/');
        if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}/")
        }
    }

    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.endpoint.is_some() || self.account_id.is_some(),
            "[database.offsite] needs account_id (or endpoint)"
        );
        if let Some(id) = &self.account_id {
            // The id becomes part of a host name.
            anyhow::ensure!(
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "account_id in [database.offsite] must be letters, digits and dashes only"
            );
        }
        anyhow::ensure!(
            !self.bucket.trim().is_empty(),
            "bucket in [database.offsite] must not be empty"
        );
        anyhow::ensure!(
            !self.access_key_id.trim().is_empty() && !self.secret_access_key.trim().is_empty(),
            "access_key_id and secret_access_key in [database.offsite] must not be empty"
        );
        anyhow::ensure!(
            self.keep >= 1,
            "keep in [database.offsite] must be at least 1"
        );
        Ok(())
    }
}

// Written by hand to keep the secret out of logs.
impl fmt::Debug for Offsite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Offsite")
            .field("account_id", &self.account_id)
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("prefix", &self.prefix)
            .field("keep", &self.keep)
            .finish()
    }
}

impl Database {
    /// The backup directory: `backup_dir`, or `backups` next to the database.
    pub fn backup_dir(&self) -> PathBuf {
        match &self.backup_dir {
            Some(dir) => PathBuf::from(dir),
            None => Path::new(&self.path)
                .parent()
                .unwrap_or(Path::new(""))
                .join("backups"),
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

/// The `[home_assistant]` section. It has two independent halves:
///
/// - the MQTT **sink** (`mqtt_*`, `discovery_prefix`, ...): solar-dash
///   publishes its readings to Home Assistant. On when `mqtt_host` is set.
/// - the **source** (`[home_assistant.source]`): solar-dash reads entity
///   states from Home Assistant. On when that sub-table exists.
///
/// Either half, or both, may be used. solar-dash never controls anything in
/// Home Assistant: the sink only publishes and the source only reads.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistant {
    /// IP address or host name of the MQTT broker Home Assistant uses. This
    /// may be the same broker as `[teslamate]`'s. No default. Without it the
    /// MQTT sink is off (a source-only setup).
    pub mqtt_host: Option<String>,
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,
    /// Broker login. Give both or neither.
    pub mqtt_username: Option<String>,
    pub mqtt_password: Option<String>,
    /// Home Assistant's MQTT discovery prefix (its default is "homeassistant").
    #[serde(default = "default_ha_discovery_prefix")]
    pub discovery_prefix: String,
    /// The start of solar-dash's own topics: `<base_topic>/status`,
    /// `<base_topic>/live` and so on.
    #[serde(default = "default_ha_base_topic")]
    pub base_topic: String,
    /// The name of the device that groups the sensors in Home Assistant.
    #[serde(default = "default_ha_device_name")]
    pub device_name: String,
    /// At most one state message per this many seconds. The inverter is read
    /// every 15 s, so the default publishes every reading.
    #[serde(default = "default_ha_min_interval")]
    pub min_interval_secs: u64,
    /// Reads entity states from Home Assistant. `None` means it is off.
    pub source: Option<HaSource>,
}

impl HomeAssistant {
    /// Whether the MQTT sink is on.
    pub fn mqtt_enabled(&self) -> bool {
        self.mqtt_host.is_some()
    }

    fn check(&self) -> Result<()> {
        match &self.mqtt_host {
            Some(host) => anyhow::ensure!(
                !host.trim().is_empty(),
                "mqtt_host in [home_assistant] must not be empty"
            ),
            None => {
                anyhow::ensure!(
                    self.source.is_some(),
                    "[home_assistant] needs mqtt_host (to publish to Home Assistant) \
                     or a [home_assistant.source] table (to read from it), or both"
                );
                // A source-only setup: any MQTT setting would be silently
                // ignored, so say so instead.
                anyhow::ensure!(
                    self.mqtt_username.is_none()
                        && self.mqtt_password.is_none()
                        && self.mqtt_port == default_mqtt_port()
                        && self.discovery_prefix == default_ha_discovery_prefix()
                        && self.base_topic == default_ha_base_topic()
                        && self.device_name == default_ha_device_name()
                        && self.min_interval_secs == default_ha_min_interval(),
                    "[home_assistant] has MQTT settings but no mqtt_host; \
                     add mqtt_host or remove them"
                );
            }
        }
        if let Some(source) = &self.source {
            source.check()?;
        }
        anyhow::ensure!(
            self.mqtt_port != 0,
            "mqtt_port in [home_assistant] must not be 0"
        );
        anyhow::ensure!(
            self.mqtt_username.is_some() == self.mqtt_password.is_some(),
            "[home_assistant] needs both mqtt_username and mqtt_password, or neither"
        );
        anyhow::ensure!(
            !self.device_name.trim().is_empty(),
            "device_name in [home_assistant] must not be empty"
        );
        anyhow::ensure!(
            self.min_interval_secs >= 1,
            "min_interval_secs in [home_assistant] must be at least 1"
        );
        for (name, topic) in [
            ("discovery_prefix", &self.discovery_prefix),
            ("base_topic", &self.base_topic),
        ] {
            // `+` and `#` are MQTT wildcards, which cannot be part of a topic
            // we publish to; a leading or trailing `/` makes empty levels.
            anyhow::ensure!(
                !topic.is_empty()
                    && !topic.starts_with('/')
                    && !topic.ends_with('/')
                    && !topic.contains(['+', '#', ' '])
                    && !topic.contains("//"),
                "{name} in [home_assistant] must be a plain topic like \"solar-dash\" \
                 (no spaces, `+`, `#` or empty levels)"
            );
        }
        Ok(())
    }
}

/// Hides the broker password, like the other secrets in this file.
impl fmt::Debug for HomeAssistant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HomeAssistant")
            .field("mqtt_host", &self.mqtt_host)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_username", &self.mqtt_username)
            .field(
                "mqtt_password",
                &self.mqtt_password.as_ref().map(|_| "<redacted>"),
            )
            .field("discovery_prefix", &self.discovery_prefix)
            .field("base_topic", &self.base_topic)
            .field("device_name", &self.device_name)
            .field("min_interval_secs", &self.min_interval_secs)
            .field("source", &self.source)
            .finish()
    }
}

/// The `[home_assistant.source]` table: how to reach Home Assistant's
/// WebSocket API and which entities to read. Read-only: solar-dash never
/// calls a service or writes a state.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaSource {
    /// Where Home Assistant is, like `http://homeassistant.local:8123`
    /// (`https://` for TLS). The WebSocket address is worked out from it.
    pub url: String,
    /// A long-lived access token (Profile page in Home Assistant). A secret:
    /// hidden in debug output and never logged.
    pub token: String,
    /// The entities to read, as repeated `[[home_assistant.source.entity]]`.
    #[serde(default)]
    pub entity: Vec<HaEntity>,
    /// How many days of Home Assistant's long-term statistics to copy in for
    /// each entity, the first time it is seen. 0 turns the backfill off.
    #[serde(default = "default_ha_backfill_days")]
    pub backfill_days: u32,
}

/// One `[[home_assistant.source.entity]]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaEntity {
    /// Home Assistant's id, like `sensor.solar_power`.
    pub entity_id: String,
    /// Optional: one of the dashboard's own metric names (such as `pv_w`),
    /// so this entity stands in for a source the home does not have. The
    /// value is also stored as that metric under the source name
    /// `home-assistant`.
    pub metric: Option<crate::metric::Metric>,
    /// A friendlier name for lists. Default: the entity id.
    pub name: Option<String>,
    /// Overrides the unit Home Assistant reports (`unit_of_measurement`),
    /// for sensors that report none.
    pub unit: Option<String>,
}

impl HaSource {
    /// The WebSocket address (`ws://host:8123/api/websocket`) for `url`.
    pub fn websocket_url(&self) -> Result<String> {
        let url = reqwest::Url::parse(self.url.trim()).with_context(|| {
            format!(
                "url in [home_assistant.source] is not a URL: {:?}",
                self.url
            )
        })?;
        let scheme = match url.scheme() {
            "http" | "ws" => "ws",
            "https" | "wss" => "wss",
            other => anyhow::bail!(
                "url in [home_assistant.source] must start with http:// or https://, not {other}://"
            ),
        };
        let host = url
            .host_str()
            .context("url in [home_assistant.source] has no host")?;
        // IPv6 hosts come back from `host_str` already in brackets.
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let path = url.path().trim_end_matches('/');
        Ok(format!("{scheme}://{host}{port}{path}/api/websocket"))
    }

    fn check(&self) -> Result<()> {
        self.websocket_url()?;
        anyhow::ensure!(
            self.backfill_days <= MAX_HA_BACKFILL_DAYS,
            "backfill_days in [home_assistant.source] must be at most {MAX_HA_BACKFILL_DAYS} (0 turns it off)"
        );
        let url = reqwest::Url::parse(self.url.trim())?;
        anyhow::ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "url in [home_assistant.source] must not have a query or fragment"
        );
        anyhow::ensure!(
            !self.token.trim().is_empty(),
            "token in [home_assistant.source] must not be empty \
             (make a long-lived access token on your Home Assistant profile page)"
        );
        anyhow::ensure!(
            !self.entity.is_empty(),
            "[home_assistant.source] needs at least one [[home_assistant.source.entity]]"
        );
        let mut seen = std::collections::HashSet::new();
        let mut metrics = std::collections::HashSet::new();
        for entity in &self.entity {
            let id = &entity.entity_id;
            // Home Assistant ids are `<domain>.<object_id>`, lower case.
            let valid = id.split_once('.').is_some_and(|(domain, object)| {
                let ok = |part: &str| {
                    !part.is_empty()
                        && part
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                };
                ok(domain) && ok(object)
            });
            anyhow::ensure!(
                valid,
                "entity_id {id:?} in [home_assistant.source] must look like \
                 \"sensor.solar_power\" (lower case, one dot)"
            );
            anyhow::ensure!(
                seen.insert(id.as_str()),
                "entity_id {id:?} is listed twice in [home_assistant.source]"
            );
            if let Some(metric) = entity.metric {
                anyhow::ensure!(
                    metrics.insert(metric),
                    "metric {:?} is given to more than one entity in [home_assistant.source]; \
                     a metric can only come from one entity",
                    metric.name()
                );
            }
            for (field, value) in [("name", &entity.name), ("unit", &entity.unit)] {
                anyhow::ensure!(
                    value.as_deref().is_none_or(|v| !v.trim().is_empty()),
                    "{field} of {id:?} in [home_assistant.source] must not be empty"
                );
            }
        }
        Ok(())
    }
}

/// Hides the access token, like the other secrets in this file.
impl fmt::Debug for HaSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HaSource")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .field("entity", &self.entity)
            .field("backfill_days", &self.backfill_days)
            .finish()
    }
}

/// Ten years: more than Home Assistant keeps by default.
const MAX_HA_BACKFILL_DAYS: u32 = 3650;

fn default_ha_backfill_days() -> u32 {
    365
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_ha_discovery_prefix() -> String {
    "homeassistant".to_string()
}

fn default_ha_base_topic() -> String {
    "solar-dash".to_string()
}

fn default_ha_device_name() -> String {
    "solar-dash".to_string()
}

fn default_ha_min_interval() -> u64 {
    15
}

/// Amber Electric's API; see config.example.toml.
#[derive(Clone, Deserialize)]
pub struct Amber {
    /// Personal API token from the Amber app. A secret.
    pub token: String,
    /// Which site (meter) to use. Optional: with exactly one site on the
    /// account that one is used.
    pub site_id: Option<String>,
    /// How far back `serve` fills in past prices (a `YYYY-MM-DD` date).
    /// Optional: the default is two years back.
    pub backfill_from: Option<chrono::NaiveDate>,
    #[serde(default = "default_amber_base_url")]
    pub base_url: String,
    /// The made-up plan behind the Bill page's "What would Amber cost?".
    #[serde(default)]
    pub compare: AmberCompare,
}

/// What an Amber plan would charge besides energy, for the Bill page's
/// "What would Amber cost?" estimate. The defaults are placeholders: copy the
/// real figures from your Amber statement.
#[derive(Debug, Clone, Deserialize)]
pub struct AmberCompare {
    /// Daily supply charge in cents.
    #[serde(default = "default_compare_supply")]
    pub supply_cents_per_day: f64,
    /// Amber's own daily fee in cents (its monthly fee divided by the days).
    #[serde(default)]
    pub fee_cents_per_day: f64,
    /// Same meaning as `feed_in_paid_when_negative` on a dynamic plan.
    #[serde(default = "default_true")]
    pub feed_in_paid_when_negative: bool,
}

impl Default for AmberCompare {
    fn default() -> Self {
        Self {
            supply_cents_per_day: default_compare_supply(),
            fee_cents_per_day: 0.0,
            feed_in_paid_when_negative: true,
        }
    }
}

fn default_compare_supply() -> f64 {
    100.0
}

impl Amber {
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !self.token.trim().is_empty(),
            "token in [amber] must not be empty"
        );
        if let Some(site_id) = &self.site_id {
            anyhow::ensure!(
                !site_id.trim().is_empty(),
                "site_id in [amber] must not be empty (leave it out to pick the only site)"
            );
        }
        anyhow::ensure!(
            self.compare.supply_cents_per_day.is_finite()
                && self.compare.supply_cents_per_day >= 0.0
                && self.compare.fee_cents_per_day.is_finite()
                && self.compare.fee_cents_per_day >= 0.0,
            "supply_cents_per_day and fee_cents_per_day in [amber.compare] must not be negative"
        );
        crate::amber::client::check_base_url(&self.base_url).context("base_url in [amber]")
    }
}

// Written by hand for the same reason as `IsolarCloud`: keep the token out of logs.
impl fmt::Debug for Amber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Amber")
            .field("token", &"<redacted>")
            .field("site_id", &self.site_id)
            .field("base_url", &self.base_url)
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
    /// Optional: price energy from stored live prices (such as Amber's)
    /// instead of the windows above, which then only fill gaps. Left out of
    /// the stored key when unset, so static plans keep their stored costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynamicPrices>,
}

/// Marks a plan as priced from the `price_interval` table. Rows are chosen by
/// `source` and stay read-only: nothing here writes to the price provider.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DynamicPrices {
    /// Only price rows whose `source` starts with this are used, for example
    /// `amber` matches `amber:<site id>`.
    pub source: String,
    /// The retailer's own daily fee, added to `supply_cents_per_day`.
    #[serde(default)]
    pub fee_cents_per_day: f64,
    /// How to read a feed-in price. Amber's API reports feed-in so that a
    /// NEGATIVE price means you are paid (this is what Home Assistant's Amber
    /// integration assumes); with `true` the stored price is flipped, so -8
    /// earns 8 c/kWh and +3 costs 3 c/kWh. Set `false` if the price is
    /// already "what you are paid". This is a setting, not a constant,
    /// because it is still to be confirmed against real data (amber-probe).
    #[serde(default = "default_true")]
    pub feed_in_paid_when_negative: bool,
}

fn default_true() -> bool {
    true
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
        if let Some(d) = &self.dynamic {
            anyhow::ensure!(
                !d.source.trim().is_empty(),
                "dynamic.source in {section} must not be empty"
            );
            anyhow::ensure!(
                d.fee_cents_per_day.is_finite(),
                "dynamic.fee_cents_per_day in {section} must be a number"
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

pub(crate) fn default_points() -> Vec<i64> {
    vec![
        83033, 83067, 83106, 83328, 83326, 83252, 83022, 83102, 83072, 83118, 83322, 83323, 83097,
    ]
}

fn default_amber_base_url() -> String {
    "https://api.amber.com.au/v1".to_string()
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

fn default_backup_keep() -> usize {
    14
}

fn default_offsite_prefix() -> String {
    "solar-dash/".to_string()
}

fn default_offsite_keep() -> usize {
    15
}

impl Config {
    /// The site's time zone: `[site] timezone`, else `[isolarcloud] timezone`
    /// (older configs), else the default.
    pub fn timezone(&self) -> Result<chrono_tz::Tz> {
        let name = self
            .site
            .as_ref()
            .and_then(|site| site.timezone.clone())
            .or_else(|| {
                self.isolarcloud
                    .as_ref()
                    .map(|cloud| cloud.timezone.clone())
            })
            .unwrap_or_else(default_timezone);
        name.parse()
            .map_err(|_| anyhow::anyhow!("unknown timezone {name:?} in config.toml"))
    }

    /// The `[isolarcloud]` section, or a clear error for commands that need it.
    pub fn require_isolarcloud(&self, command: &str) -> Result<&IsolarCloud> {
        self.isolarcloud.as_ref().with_context(|| {
            format!(
                "{command} needs an [isolarcloud] section in the config \
                 (see config.example.toml)"
            )
        })
    }

    /// The rules from `[sources]`, with the defaults filled in.
    pub fn source_rules(&self) -> Result<crate::hub::SourceRules> {
        crate::hub::SourceRules::from_config(&self.sources)
    }

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
        config
            .source_rules()
            .with_context(|| format!("invalid config file {}", path.display()))?;
        if let Some(offsite) = &config.database.offsite {
            offsite
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
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
        if let Some(home_assistant) = &config.home_assistant {
            home_assistant
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        if let Some(globird) = &config.globird {
            globird
                .check()
                .with_context(|| format!("invalid config file {}", path.display()))?;
        }
        if let Some(amber) = &config.amber {
            amber
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
    fn sources_section_is_optional_and_checked() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.source_rules().unwrap(), Default::default());

        let text = format!(
            "{MINIMAL}\n[sources]\ndaily_energy = [\"isolarcloud\"]\nfinished_day_coverage = 0.95\n"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        let rules = cfg.source_rules().unwrap();
        assert!(!rules.live_enabled());
        assert_eq!(rules.finished_day_coverage(), 0.95);

        let text = format!("{MINIMAL}\n[sources]\ndaily_energy = [\"solarweb\"]\n");
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = format!("{:#}", cfg.source_rules().unwrap_err());
        assert!(err.contains("sungrow-modbus, isolarcloud"), "{err}");

        // A misspelt key is an error, not silently ignored.
        let text = format!("{MINIMAL}\n[sources]\ndaily_energies = []\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        assert_eq!(cfg.isolarcloud.as_ref().unwrap().appkey, "YOUR_APPKEY");
    }

    #[test]
    fn defaults_apply_when_sections_omitted() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(
            cfg.isolarcloud.as_ref().unwrap().gateway,
            "https://augateway.isolarcloud.com"
        );
        assert!(cfg.isolarcloud.as_ref().unwrap().encrypt);
        assert_eq!(cfg.server.bind, "127.0.0.1:8080");
        assert_eq!(cfg.database.path, "data/solar.db");
        assert_eq!(cfg.database.backup_dir(), PathBuf::from("data/backups"));
        assert_eq!(cfg.database.backup_keep, 14);
        assert_eq!(
            cfg.isolarcloud.as_ref().unwrap().timezone,
            "Australia/Adelaide"
        );
        assert_eq!(cfg.isolarcloud.as_ref().unwrap().points.len(), 13);
        assert!(cfg.isolarcloud.as_ref().unwrap().points.contains(&83252));
    }

    const OFFSITE: &str = "[database.offsite]\naccount_id = \"abc123\"\nbucket = \"b\"\n\
                           access_key_id = \"AK\"\nsecret_access_key = \"SECRET-VALUE\"\n";

    #[test]
    fn offsite_is_off_by_default_and_has_defaults() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.database.offsite.is_none());

        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{OFFSITE}")).unwrap();
        let off = cfg.database.offsite.unwrap();
        off.check().unwrap();
        assert_eq!(off.endpoint(), "https://abc123.r2.cloudflarestorage.com");
        assert_eq!(off.prefix(), "solar-dash/");
        assert_eq!(off.keep, 15);
        // The section nests under [database], so the other settings still apply.
        assert_eq!(cfg.database.backup_keep, 14);
    }

    #[test]
    fn offsite_settings_can_be_changed_and_are_checked() {
        let text = |extra: &str| format!("{MINIMAL}\n{OFFSITE}{extra}");
        let parse = |extra: &str| {
            toml::from_str::<Config>(&text(extra))
                .unwrap()
                .database
                .offsite
                .unwrap()
        };
        let off = parse("keep = 3\nprefix = \"/a/b\"\nendpoint = \"http://127.0.0.1:9000/\"\n");
        off.check().unwrap();
        assert_eq!(off.keep, 3);
        assert_eq!(off.prefix(), "a/b/");
        assert_eq!(off.endpoint(), "http://127.0.0.1:9000");
        assert_eq!(parse("prefix = \"\"\n").prefix(), "");

        assert!(parse("keep = 0\n").check().is_err());
        let no_bucket = OFFSITE.replace("bucket = \"b\"", "bucket = \"\"");
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{no_bucket}")).unwrap();
        assert!(cfg.database.offsite.unwrap().check().is_err());
        let no_where = "[database.offsite]\nbucket = \"b\"\naccess_key_id = \"a\"\nsecret_access_key = \"s\"\n";
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{no_where}")).unwrap();
        assert!(cfg.database.offsite.unwrap().check().is_err());
        let bad_id = OFFSITE.replace("abc123", "evil.example/");
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{bad_id}")).unwrap();
        assert!(cfg.database.offsite.unwrap().check().is_err());
    }

    #[test]
    fn offsite_debug_hides_the_secret() {
        let cfg: Config = toml::from_str(&format!("{MINIMAL}\n{OFFSITE}")).unwrap();
        let text = format!("{:?}", cfg.database);
        assert!(!text.contains("SECRET-VALUE"));
        assert!(text.contains("<redacted>"));
        assert!(text.contains("AK"));
    }

    #[test]
    fn backup_settings_can_be_changed() {
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[database]\npath = \"/var/lib/x/solar.db\"\nbackup_keep = 0\n"
        ))
        .unwrap();
        assert_eq!(cfg.database.backup_keep, 0);
        // Next to the database unless `backup_dir` says otherwise.
        assert_eq!(
            cfg.database.backup_dir(),
            PathBuf::from("/var/lib/x/backups")
        );
        let cfg: Config = toml::from_str(&format!(
            "{MINIMAL}\n[database]\nbackup_dir = \"/mnt/usb\"\n"
        ))
        .unwrap();
        assert_eq!(cfg.database.backup_dir(), PathBuf::from("/mnt/usb"));
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
    fn amber_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.amber.is_none());
    }

    #[test]
    fn amber_section_needs_a_token_and_has_defaults() {
        let ok = format!("{MINIMAL}\n[amber]\ntoken = \"psk_abc\"\n");
        let amber = toml::from_str::<Config>(&ok).unwrap().amber.unwrap();
        assert_eq!(amber.token, "psk_abc");
        assert!(amber.site_id.is_none());
        assert_eq!(amber.base_url, "https://api.amber.com.au/v1");
        assert!(amber.check().is_ok());

        let no_token = format!("{MINIMAL}\n[amber]\nsite_id = \"x\"\n");
        assert!(toml::from_str::<Config>(&no_token).is_err());

        let parse = |extra: &str| -> Amber {
            let text = format!("{MINIMAL}\n[amber]\ntoken = \"t\"\n{extra}");
            toml::from_str::<Config>(&text).unwrap().amber.unwrap()
        };
        assert!(parse("site_id = \"01ABC\"").check().is_ok());
        assert!(parse("site_id = \"\"").check().is_err());
        assert_eq!(parse("").backfill_from, None);
        assert_eq!(
            parse("backfill_from = \"2024-03-01\"").backfill_from,
            chrono::NaiveDate::from_ymd_opt(2024, 3, 1)
        );
        let bad_date = format!("{MINIMAL}\n[amber]\ntoken = \"t\"\nbackfill_from = \"soon\"\n");
        assert!(toml::from_str::<Config>(&bad_date).is_err());
        assert!(parse("base_url = \"not a url\"").check().is_err());
        assert!(parse("base_url = \"ftp://x\"").check().is_err());
        assert!(
            parse("base_url = \"http://127.0.0.1:9/v1\"")
                .check()
                .is_ok()
        );
        let empty = Amber {
            token: " ".into(),
            ..parse("")
        };
        assert!(empty.check().is_err());
        // [amber.compare] is optional and has documented defaults.
        let defaults = parse("").compare;
        assert_eq!(defaults.supply_cents_per_day, 100.0);
        assert_eq!(defaults.fee_cents_per_day, 0.0);
        assert!(defaults.feed_in_paid_when_negative);
        let set = parse("[amber.compare]\nsupply_cents_per_day = 90.0\nfee_cents_per_day = 80.0\n");
        assert_eq!(set.compare.fee_cents_per_day, 80.0);
        assert!(
            parse("[amber.compare]\nfee_cents_per_day = -1.0")
                .check()
                .is_err()
        );
    }

    #[test]
    fn amber_token_is_not_in_debug_output() {
        let amber = Amber {
            token: "psk_supersecret".into(),
            site_id: Some("01ABC".into()),
            backfill_from: None,
            base_url: default_amber_base_url(),
            compare: AmberCompare::default(),
        };
        let text = format!("{amber:?}");
        assert!(!text.contains("psk_supersecret"));
        assert!(text.contains("<redacted>"));
        assert!(text.contains("01ABC"));
    }

    fn parse_ha(extra: &str) -> HomeAssistant {
        let text = format!("{MINIMAL}\n[home_assistant]\nmqtt_host = \"broker\"\n{extra}");
        toml::from_str::<Config>(&text)
            .unwrap()
            .home_assistant
            .unwrap()
    }

    #[test]
    fn home_assistant_is_off_without_a_section() {
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert!(cfg.home_assistant.is_none());
    }

    #[test]
    fn home_assistant_defaults_apply() {
        let ha = parse_ha("");
        assert_eq!(ha.mqtt_host.as_deref(), Some("broker"));
        assert_eq!(ha.mqtt_port, 1883);
        assert_eq!(ha.mqtt_username, None);
        assert_eq!(ha.discovery_prefix, "homeassistant");
        assert_eq!(ha.base_topic, "solar-dash");
        assert_eq!(ha.device_name, "solar-dash");
        assert_eq!(ha.min_interval_secs, 15);
        assert!(ha.check().is_ok());
    }

    #[test]
    fn home_assistant_section_needs_a_host_or_a_source() {
        let text = format!("{MINIMAL}\n[home_assistant]\nmqtt_port = 1883\n");
        let cfg: Config = toml::from_str(&text).unwrap();
        let err = cfg.home_assistant.unwrap().check().unwrap_err().to_string();
        assert!(err.contains("mqtt_host") && err.contains("source"), "{err}");
    }

    const SOURCE_ONLY: &str = "[home_assistant.source]\n\
        url = \"http://homeassistant.local:8123\"\ntoken = \"secret-token\"\n\
        [[home_assistant.source.entity]]\nentity_id = \"sensor.solar_power\"\n\
        metric = \"pv_w\"\nname = \"Solar\"\nunit = \"W\"\n\
        [[home_assistant.source.entity]]\nentity_id = \"sensor.outdoor_temp\"\n";

    fn parse_source(text: &str) -> HomeAssistant {
        toml::from_str::<Config>(&format!("{MINIMAL}\n{text}"))
            .unwrap()
            .home_assistant
            .unwrap()
    }

    #[test]
    fn home_assistant_source_only_is_valid_and_has_no_mqtt() {
        let ha = parse_source(SOURCE_ONLY);
        assert!(ha.check().is_ok());
        assert!(!ha.mqtt_enabled());
        let source = ha.source.as_ref().unwrap();
        assert_eq!(source.entity.len(), 2);
        assert_eq!(source.backfill_days, 365);
        assert_eq!(source.entity[0].metric, Some(crate::metric::Metric::PvW));
        assert_eq!(source.entity[1].metric, None);
        assert_eq!(
            source.websocket_url().unwrap(),
            "ws://homeassistant.local:8123/api/websocket"
        );
    }

    #[test]
    fn home_assistant_mqtt_and_source_can_be_combined() {
        let ha = parse_source(&format!(
            "[home_assistant]\nmqtt_host = \"b\"\n{SOURCE_ONLY}"
        ));
        assert!(ha.check().is_ok());
        assert!(ha.mqtt_enabled() && ha.source.is_some());
    }

    #[test]
    fn home_assistant_mqtt_settings_need_a_host() {
        let ha = parse_source(&format!(
            "[home_assistant]\nmqtt_username = \"u\"\nmqtt_password = \"p\"\n{SOURCE_ONLY}"
        ));
        assert!(ha.check().unwrap_err().to_string().contains("mqtt_host"));
    }

    #[test]
    fn websocket_url_follows_the_scheme() {
        let url = |u: &str| {
            HaSource {
                url: u.into(),
                token: "t".into(),
                entity: vec![],
                backfill_days: 365,
            }
            .websocket_url()
        };
        assert_eq!(
            url("https://ha.example.com").unwrap(),
            "wss://ha.example.com/api/websocket"
        );
        assert_eq!(
            url("http://10.0.0.5:8123/").unwrap(),
            "ws://10.0.0.5:8123/api/websocket"
        );
        assert_eq!(
            url("https://example.com/ha/").unwrap(),
            "wss://example.com/ha/api/websocket"
        );
        assert!(url("ftp://x").unwrap_err().to_string().contains("http://"));
        assert!(url("homeassistant.local:8123").is_err());
        assert!(url("not a url").is_err());
    }

    #[test]
    fn home_assistant_source_is_validated() {
        let bad = |edit: &dyn Fn(&str) -> String| -> String {
            parse_source(&edit(SOURCE_ONLY))
                .check()
                .unwrap_err()
                .to_string()
        };
        assert!(bad(&|s| s.replace("token = \"secret-token\"", "token = \" \"")).contains("token"));
        assert!(bad(&|s| s.replace("http://homeassistant.local:8123", "ftp://h")).contains("http"));
        assert!(
            bad(&|s| s.replace("http://homeassistant.local:8123", "http://h/?a=1"))
                .contains("query")
        );
        assert!(bad(&|s| s.replace("sensor.outdoor_temp", "Sensor Outdoor")).contains("entity_id"));
        assert!(bad(&|s| s.replace("sensor.outdoor_temp", "sensor.solar_power")).contains("twice"));
        assert!(bad(&|s| s.replace("name = \"Solar\"", "name = \" \"")).contains("name"));
        let twice = format!(
            "{SOURCE_ONLY}[[home_assistant.source.entity]]\nentity_id = \"sensor.other\"\nmetric = \"pv_w\"\n"
        );
        let err = parse_source(&twice).check().unwrap_err().to_string();
        assert!(err.contains("more than one entity"), "{err}");
        assert!(
            bad(&|s| s.replace("token =", "backfill_days = 5000\ntoken ="))
                .contains("backfill_days")
        );
        let none = parse_source("[home_assistant.source]\nurl = \"http://h\"\ntoken = \"t\"\n");
        assert!(
            none.check()
                .unwrap_err()
                .to_string()
                .contains("at least one")
        );
    }

    #[test]
    fn unknown_metric_name_is_a_parse_error() {
        let text = SOURCE_ONLY.replace("pv_w", "solar_watts");
        assert!(toml::from_str::<Config>(&format!("{MINIMAL}\n{text}")).is_err());
    }

    #[test]
    fn home_assistant_token_is_not_in_debug_output() {
        let ha = parse_source(SOURCE_ONLY);
        let text = format!("{ha:?}");
        assert!(!text.contains("secret-token"));
        assert!(text.contains("<redacted>") && text.contains("sensor.solar_power"));
    }

    #[test]
    fn home_assistant_is_validated() {
        let bad = |extra: &str| parse_ha(extra).check().unwrap_err().to_string();
        assert!(bad("mqtt_username = \"u\"").contains("both"));
        assert!(bad("mqtt_port = 0").contains("mqtt_port"));
        assert!(bad("min_interval_secs = 0").contains("min_interval_secs"));
        assert!(bad("device_name = \" \"").contains("device_name"));
        for topic in ["", "/x", "x/", "a//b", "a/+", "a/#", "a b"] {
            assert!(
                bad(&format!("base_topic = \"{topic}\"")).contains("base_topic"),
                "{topic:?}"
            );
        }
        assert!(bad("discovery_prefix = \"#\"").contains("discovery_prefix"));
        let ok =
            parse_ha("mqtt_username = \"u\"\nmqtt_password = \"p\"\nbase_topic = \"home/solar\"");
        assert!(ok.check().is_ok());
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

    const CLOUD_TZ: &str = r#"
        [isolarcloud]
        appkey = "k"
        access_key = "s"
        rsa_public_key = "r"
        username = "u"
        password = "p"
        timezone = "Australia/Perth"
    "#;

    #[test]
    fn isolarcloud_section_is_optional_and_site_sets_the_timezone() {
        let cfg: Config = toml::from_str("[site]\ntimezone = \"Australia/Sydney\"\n").unwrap();
        assert!(cfg.isolarcloud.is_none());
        assert_eq!(cfg.timezone().unwrap(), chrono_tz::Australia::Sydney);
    }

    #[test]
    fn old_config_keeps_the_isolarcloud_timezone() {
        let cfg: Config = toml::from_str(CLOUD_TZ).unwrap();
        assert_eq!(cfg.timezone().unwrap(), chrono_tz::Australia::Perth);
    }

    #[test]
    fn site_timezone_wins_over_isolarcloud() {
        let text = format!("{CLOUD_TZ}\n[site]\ntimezone = \"Australia/Sydney\"\n");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.timezone().unwrap(), chrono_tz::Australia::Sydney);
    }

    #[test]
    fn no_timezone_anywhere_gives_the_default() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.timezone().unwrap(), chrono_tz::Australia::Adelaide);
        let cfg: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(cfg.timezone().unwrap(), chrono_tz::Australia::Adelaide);
    }

    #[test]
    fn bad_timezone_is_an_error() {
        let cfg: Config = toml::from_str("[site]\ntimezone = \"Mars/Base\"\n").unwrap();
        assert!(cfg.timezone().is_err());
    }

    #[test]
    fn cloud_commands_get_a_clear_error_without_the_section() {
        let cfg: Config = toml::from_str("").unwrap();
        let err = cfg.require_isolarcloud("backfill").unwrap_err().to_string();
        assert!(err.contains("needs an [isolarcloud] section"), "{err}");
    }
}
