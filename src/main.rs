mod config;
mod cost;
mod db;
mod ingest;
mod isolar;
mod modbus;
mod teslamate;
mod web;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use serde_json::Value;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
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
    let listener = TcpListener::bind(&config.server.bind)
        .await
        .with_context(|| format!("could not listen on {}", config.server.bind))?;

    // Settings that no retry can fix fail here, before anything starts. The
    // login itself happens in the live sync, which retries with backoff: the
    // dashboard and Modbus data do not need iSolarCloud to be up.
    ingest::check_sync_config(&config.isolarcloud)?;
    let client = Client::new(config.isolarcloud.clone())?;

    info!(address = %config.server.bind, "web server started");
    // The Modbus poller only runs if there is a [modbus] section. Otherwise
    // this future finishes at once, which `try_join!` is happy with.
    let latest = state.latest();
    let poll_modbus = async {
        match config.modbus.clone() {
            Some(modbus) => modbus::run_poll(modbus, &pool, latest, shutdown_signal()).await,
            None => Ok(()),
        }
    };
    // The TeslaMate reader works the same way: it only runs if there is a
    // [teslamate] section, and its errors are only logged. It saves charging
    // sessions, so it gets the pool.
    let car = state.car();
    let follow_car = async {
        if let Some(teslamate) = config.teslamate.clone()
            && let Err(err) = teslamate::run(teslamate, car, pool.clone(), shutdown_signal()).await
        {
            error!("TeslaMate reader stopped: {err:#}; the dashboard keeps running");
        }
        Ok::<(), anyhow::Error>(())
    };
    // The live sync must never take the others down: if it fails for a
    // fatal reason (say a database error) we log it and keep serving.
    let live_sync = async {
        if let Err(err) =
            ingest::run_live_sync(client, &pool, &config.isolarcloud, shutdown_signal()).await
        {
            error!("live sync stopped: {err:#}; the dashboard keeps running");
        }
        Ok::<(), anyhow::Error>(())
    };
    // Run all four until Ctrl+C. `try_join!` polls them together on this
    // task and returns early if one fails (neither the live sync, the
    // Modbus poller nor the TeslaMate reader does: their errors are only
    // logged). Each gets its own `ctrl_c` future; one Ctrl+C wakes them all.
    tokio::try_join!(
        live_sync,
        web::serve(listener, state, shutdown_signal()),
        poll_modbus,
        follow_car,
    )?;
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

/// Opens the database, logs in and downloads history starting at `from`.
async fn run_backfill(config: Config, from: &str) -> Result<()> {
    // Check the date first, so a typo fails before any network call.
    let from = NaiveDate::parse_from_str(from, "%Y-%m-%d")
        .with_context(|| format!("--from must look like 2023-02-21, got {from:?}"))?;
    let pool = db::open(&config.database.path).await?;
    let mut client = Client::new(config.isolarcloud.clone())?;
    client.login().await?;
    ingest::backfill(&client, &pool, &config.isolarcloud, from).await
}

/// Logs in, calls one endpoint and prints its `result_data` as pretty JSON.
async fn api_call(config: Config, path: &str, params: Option<&str>) -> Result<()> {
    // Check the params before logging in, so a typo fails without a network call.
    let params: Value = serde_json::from_str(params.unwrap_or("{}"))
        .context("params must be valid JSON, e.g. '{\"curPage\":1}'")?;
    if !params.is_object() {
        bail!("params must be a JSON object, e.g. '{{\"curPage\":1}}'");
    }

    let mut client = Client::new(config.isolarcloud)?;
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
        let sample = reader.read_sample().await?;
        println!("{}", modbus::describe(&sample));
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
            Ok(sample) => {
                tally.ok += 1;
                println!("{now}  {}", modbus::describe(&sample));
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

/// Logs in, lists the plants and prints a short summary.
async fn login_test(config: Config) -> Result<()> {
    let config_plant_id = config.isolarcloud.plant_id;
    let mut client = Client::new(config.isolarcloud)?;
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
