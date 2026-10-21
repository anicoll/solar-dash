mod config;
mod db;
mod ingest;
mod isolar;
mod web;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
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
    /// Call any iSolarCloud endpoint and print the result (for exploring the API).
    Api {
        /// Endpoint path, e.g. /openapi/getPowerStationList.
        path: String,
        /// Params as a JSON object, e.g. '{"curPage":1,"size":10}'. Defaults to {}.
        params: Option<String>,
    },
}

// `#[tokio::main]` turns this async fn into a normal `main` that starts the
// async runtime for us.
#[tokio::main]
async fn main() -> Result<()> {
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

    let mut client = Client::new(config.isolarcloud.clone())?;
    // Failing here, with a clear error, beats failing every 5 minutes.
    client.login().await.context("login failed at startup")?;

    info!(address = %config.server.bind, "web server started");
    // Run both until Ctrl+C. `try_join!` polls them together on this task and
    // returns early if either fails. Each gets its own `ctrl_c` future; one
    // Ctrl+C wakes them both.
    tokio::try_join!(
        ingest::run_live_sync(client, &pool, &config.isolarcloud, shutdown_signal()),
        web::serve(listener, state, shutdown_signal()),
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
