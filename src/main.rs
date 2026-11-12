mod admin_data;
mod admin_import;
mod amber;
mod backup;
mod bills;
mod config;
mod cost;
mod db;
mod ev_cost;
mod fuel_log;
mod globird;
mod hub;
mod ingest;
mod isolar;
mod metric;
mod modbus;
mod offsite;
mod petrol_prices;
mod teslamate;
mod teslamate_history;
mod web;
mod whatif;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use chrono_tz::Tz;
use clap::{Parser, Subcommand};
use serde_json::Value;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use config::Config;
use isolar::client::Client;

/// Self-hosted Sungrow iSolarCloud dashboard.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the background sync and the web dashboard.
    Serve,
    /// Download historical data starting at a date.
    Backfill {
        /// First day to fetch, as YYYY-MM-DD.
        #[arg(long)]
        from: String,
    },
    /// Make the password hash for the web dashboard (needs no config file).
    HashPassword,
    /// Check that the iSolarCloud credentials work.
    LoginTest,
    /// Read the inverter over Modbus TCP and print what it says (needs a [modbus] section).
    ModbusTest {
        /// Keep reading every interval_secs until Ctrl+C, then print a summary.
        #[arg(long)]
        watch: bool,
    },
    /// Log in to the GloBird portal and save a redacted dump of what each endpoint
    /// returns (needs a [globird] section).
    GlobirdProbe {
        /// Where to write the JSON dump.
        #[arg(long, default_value = "globird-probe.json")]
        out: PathBuf,
        /// How many days of usage and cost to ask for.
        #[arg(long, default_value_t = 31)]
        days: u32,
    },
    /// Print what the Amber Electric API returns for your site: sites, current
    /// prices and recent usage (needs an [amber] section). Read-only.
    AmberProbe {
        /// Also save the raw API replies to this JSON file (for test fixtures).
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Call any iSolarCloud endpoint and print the result (for exploring the API).
    Api {
        /// Endpoint path, e.g. /openapi/getPowerStationList.
        path: String,
        /// Params as a JSON object, e.g. '{"curPage":1,"size":10}'. Defaults to {}.
        params: Option<String>,
    },
}

// This runs on a 1-2 core LXC, so build the async runtime by hand instead of
// using `#[tokio::main]` (which starts one worker per core and allows up to 512
// blocking threads). Two workers is plenty, and the blocking pool (argon2
// password checks, sqlx) only needs a few threads.
fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // RUST_LOG controls verbosity, e.g. RUST_LOG=debug. Default is info.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // This command runs before the config is loaded, so it works without a config.toml.
    if matches!(cli.command, Command::HashPassword) {
        if let Err(err) = hash_password_command() {
            eprintln!("Hashing failed: {err:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `?` returns early with the error if loading fails.
    let config = Config::load(&cli.config)?;
    info!(config = %cli.config.display(), "config loaded");
    tracing::debug!(?config, "parsed config (secrets are redacted)");

    match cli.command {
        Command::Serve => {
            if let Err(err) = run_serve(config).await {
                eprintln!("Serve failed: {err:#}");
                std::process::exit(1);
            }
        }
        Command::Backfill { from } => {
            if let Err(err) = run_backfill(config, &from).await {
                eprintln!("Backfill failed: {err:#}");
                std::process::exit(1);
            }
        }
        // Handled above, before the config was loaded.
        Command::HashPassword => unreachable!("handled before loading the config"),
        Command::LoginTest => {
            // `{:#}` prints the whole error chain on one line.
            if let Err(err) = login_test(config).await {
                eprintln!("Login test failed: {err:#}");
                std::process::exit(1);
            }
        }
        Command::ModbusTest { watch } => {
            if let Err(err) = modbus_test(config, watch).await {
                eprintln!("Modbus test failed: {err:#}");
                std::process::exit(1);
            }
        }
        Command::GlobirdProbe { out, days } => {
            if let Err(err) = globird_probe(config, &out, days).await {
                eprintln!("GloBird probe failed: {err:#}");
                std::process::exit(1);
            }
        }
        Command::AmberProbe { json } => {
            if let Err(err) = amber_probe(config, json.as_deref()).await {
                eprintln!("Amber probe failed: {err:#}");
                std::process::exit(1);
            }
        }
        Command::Api { path, params } => {
            if let Err(err) = api_call(config, &path, params.as_deref()).await {
                eprintln!("API call failed: {err:#}");
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

/// Opens the database, serves the web dashboard and keeps the data up to date
/// until Ctrl+C.
async fn run_serve(config: Config) -> Result<()> {
    let pool = db::open(&config.database.path).await?;
    info!(path = %config.database.path, "database ready");
    // Check the web settings and the port first, so mistakes fail before any network call.
    let state = web::AppState::from_config(pool.clone(), &config)?;
    // The tariffs, petrol prices, purchases, bills and preferences live in the
    // database. Copy them from an older config once, then load them.
    admin_import::import_from_config(&pool, &config).await?;
    admin_import::warn_ignored(&config);
    let admin_data = admin_data::AdminData::load(&mut *pool.acquire().await?).await?;
    let offsite_status = offsite::Status::default();
    let state = state
        .with_admin_data(admin_data)
        .with_backup_dir(config.database.backup_dir())
        .with_offsite_status(offsite_status.clone());
    let listener = TcpListener::bind(&config.server.bind)
        .await
        .with_context(|| format!("could not listen on {}", config.server.bind))?;

    // Settings that no retry can fix fail here, before anything starts. The
    // login itself happens in the live sync, which retries with backoff: the
    // dashboard and Modbus data do not need iSolarCloud to be up.
    let client = match &config.isolarcloud {
        Some(cloud) => {
            ingest::check_sync_config(cloud)?;
            Some(Client::new(cloud.clone())?)
        }
        None => {
            info!("no [isolarcloud] section: the iSolarCloud sync is off");
            None
        }
    };
    warn_unconfigured_sources(&config);

    // A mistake in the offsite settings that no retry can fix fails at
    // startup too.
    let offsite = config
        .database
        .offsite
        .as_ref()
        .map(|settings| offsite::Offsite::new(settings, offsite_status))
        .transpose()?;

    // Each background job is a spoke (see `hub`). Build them from the config,
    // then run them all next to the web server.
    let tz = state.tz();
    let spokes = hub::spokes::build(&config, &state, client, offsite);
    let ids: Vec<&str> = spokes.iter().map(|spoke| spoke.id()).collect();
    info!("spokes: {}", ids.join(", "));
    let state = state.with_enabled_spokes(&ids);

    // One Ctrl+C fires the shutdown signal that every spoke waits on.
    let (trigger, shutdown) = hub::shutdown_channel();
    tokio::spawn(async move {
        shutdown_signal().await;
        trigger.fire();
    });
    let ctx = hub::SpokeContext {
        pool,
        tz,
        shutdown,
        hub: hub::Hub::default(),
    };

    state.spawn_warmup();
    info!(address = %config.server.bind, "web server started");
    // Run until Ctrl+C. `try_join!` polls both on this task and returns early
    // if the web server fails. The spokes never fail the whole: their errors
    // are only logged.
    tokio::try_join!(web::serve(listener, state, shutdown_signal()), async {
        hub::run_spokes(spokes, ctx).await;
        Ok(())
    })?;
    Ok(())
}

/// Completes when Ctrl+C is pressed.
async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        warn!("could not listen for Ctrl+C: {err}");
    }
    info!("Ctrl+C received; stopping");
}

/// Asks for a password twice (nothing is echoed) and prints its hash.
fn hash_password_command() -> Result<()> {
    let password = rpassword::prompt_password("Password: ")?;
    let again = rpassword::prompt_password("Repeat password: ")?;
    if password != again {
        bail!("the passwords do not match");
    }
    if password.is_empty() {
        bail!("the password must not be empty");
    }
    let hash = web::hash_password(&password)?;
    println!("Paste this line into the [server] section of config.toml:");
    println!();
    println!("password_hash = \"{hash}\"");
    Ok(())
}

/// Warns when `[sources] daily_energy` names a source that is not configured.
/// That source simply never has data, so this is not an error.
fn warn_unconfigured_sources(config: &Config) {
    let Some(ids) = &config.sources.daily_energy else {
        return;
    };
    for id in ids {
        let configured = match id.as_str() {
            "sungrow-modbus" => config.modbus.is_some(),
            "isolarcloud" => config.isolarcloud.is_some(),
            _ => true,
        };
        if !configured {
            warn!(
                "[sources] daily_energy lists {id:?}, but it is not configured; it will never have data"
            );
        }
    }
}

/// Opens the database, logs in and downloads history starting at `from`.
async fn run_backfill(config: Config, from: &str) -> Result<()> {
    // Check the date first, so a typo fails before any network call.
    let from = NaiveDate::parse_from_str(from, "%Y-%m-%d")
        .with_context(|| format!("--from must look like 2024-01-01, got {from:?}"))?;
    let cloud = config.require_isolarcloud("backfill")?;
    let pool = db::open(&config.database.path).await?;
    let mut client = Client::new(cloud.clone())?;
    client.login().await?;
    ingest::backfill(&client, &pool, cloud, from).await
}

/// Logs in, calls one endpoint and prints its `result_data` as pretty JSON.
async fn api_call(config: Config, path: &str, params: Option<&str>) -> Result<()> {
    // Check the params before logging in, so a typo fails without a network call.
    let params: Value = serde_json::from_str(params.unwrap_or("{}"))
        .context("params must be valid JSON, e.g. '{\"curPage\":1}'")?;
    if !params.is_object() {
        bail!("params must be a JSON object, e.g. '{{\"curPage\":1}}'");
    }

    let mut client = Client::new(config.require_isolarcloud("api")?.clone())?;
    client.login().await?;
    let data = client.call(&with_leading_slash(path), params).await?;
    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}

/// Adds a leading `/` to an endpoint path if it is missing.
fn with_leading_slash(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// Logs in to the GloBird portal and writes a redacted dump of its endpoints.
async fn globird_probe(config: Config, out: &Path, days: u32) -> Result<()> {
    let tz: Tz = config.timezone()?;
    let globird = config.globird.context(
        "globird-probe needs a [globird] section in config.toml (see config.example.toml)",
    )?;
    // Check the settings first, so mistakes fail before any network call.
    if !(1..=366).contains(&days) {
        bail!("--days must be between 1 and 366, got {days}");
    }
    let client = globird::client::Client::new(globird)?;
    globird::probe::run(client, tz, days, out).await
}

/// Prints what the Amber API returns for the configured site.
async fn amber_probe(config: Config, json: Option<&Path>) -> Result<()> {
    let tz: Tz = config.timezone()?;
    let amber = config
        .amber
        .context("amber-probe needs an [amber] section in config.toml (see config.example.toml)")?;
    amber::probe::run(&amber, tz, json).await
}

/// Reads the inverter once, or with `watch` every `interval_secs` until Ctrl+C.
/// Failed reads are printed and counted, and reading carries on. This is the
/// stability test for the WiNet-S dongle.
async fn modbus_test(config: Config, watch: bool) -> Result<()> {
    let modbus = config
        .modbus
        .context("modbus-test needs a [modbus] section in config.toml (see config.example.toml)")?;
    let interval = Duration::from_secs(modbus.interval_secs);
    let mut reader = modbus::Reader::new(modbus);

    if !watch {
        let (sample, meter) = reader.read_sample().await?;
        println!("{}", modbus::describe(&sample));
        println!("{}", describe_meter(meter.as_ref()));
        print_settings(&mut reader).await;
        return Ok(());
    }

    let mut tally = modbus::Tally::default();
    let mut ticker = tokio::time::interval(interval);
    // If a read is slow, don't try to catch up with a burst of reads.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    println!(
        "Reading every {}s. Press Ctrl+C to stop.",
        interval.as_secs()
    );
    // The settings change rarely, so they are only shown once, at the start.
    print_settings(&mut reader).await;
    // Pinning lets `select!` poll the same future in every round.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        // The first tick completes at once, so the first read is immediate.
        tokio::select! {
            _ = &mut shutdown => break,
            _ = ticker.tick() => {}
        }
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        // Stop at once on Ctrl+C even if the read is in progress.
        let result = tokio::select! {
            _ = &mut shutdown => break,
            result = reader.read_sample() => result,
        };
        match result {
            Ok((sample, meter)) => {
                tally.ok += 1;
                println!("{now}  {}", modbus::describe(&sample));
                println!("{now}  {}", describe_meter(meter.as_ref()));
            }
            Err(err) => {
                tally.failed += 1;
                println!("{now}  FAILED: {err:#}");
            }
        }
    }
    println!("{tally}");
    Ok(())
}

/// The grid totals line, or the same line with n/a when the inverter refused them.
fn describe_meter(meter: Option<&modbus::Meter>) -> String {
    match meter {
        Some(meter) => modbus::describe_meter(meter),
        None => "grid totals: import n/a, export n/a".to_string(),
    }
}

/// Prints the inverter's control settings, or why they could not be read.
/// Settings are a bonus here, so a failure does not fail the test.
async fn print_settings(reader: &mut modbus::Reader) {
    match reader.read_settings().await {
        Ok(settings) => println!("{}", modbus::describe_settings(&settings)),
        Err(err) => println!("Settings read failed: {err:#}"),
    }
}

/// Logs in, lists the plants and prints a short summary.
async fn login_test(config: Config) -> Result<()> {
    let cloud = config.require_isolarcloud("login-test")?.clone();
    let config_plant_id = cloud.plant_id;
    let mut client = Client::new(cloud)?;
    client.login().await?;
    let plants = client.plants().await?;

    let noun = if plants.len() == 1 { "plant" } else { "plants" };
    println!("Logged in. Found {} {noun}", plants.len());
    for plant in &plants {
        println!("  {} {}", plant.ps_id, plant.ps_name);
    }
    if config_plant_id.is_none() && plants.len() > 1 {
        println!("Set plant_id in config.toml to choose one");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_missing_leading_slash() {
        assert_eq!(with_leading_slash("/openapi/login"), "/openapi/login");
        assert_eq!(with_leading_slash("openapi/login"), "/openapi/login");
    }
}
