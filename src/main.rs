mod config;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::Config;

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
    /// Check that the iSolarCloud credentials work.
    LoginTest,
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

    // `?` returns early with the error if loading fails.
    let config = Config::load(&cli.config)?;
    info!(config = %cli.config.display(), "config loaded");
    tracing::debug!(?config, "parsed config (secrets are redacted)");

    match cli.command {
        Command::Serve => info!("serve is not implemented yet"),
        Command::Backfill { from } => info!(%from, "backfill is not implemented yet"),
        Command::LoginTest => info!("login-test is not implemented yet"),
    }
    Ok(())
}
