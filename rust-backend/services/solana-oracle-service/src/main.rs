//! solana-oracle-service: the Solana stack's Pyth gateway (port 9013).
//!
//! A thin wrapper over the shared `oracle-service` engine — the Sui service
//! contains zero chain code, so the only difference here is which catalog
//! feeds are discovered from (solana-token-info instead of token-info). Pyth
//! feed ids are chain-agnostic; the SSE subscription, PriceCache,
//! BenchmarkVol, router and WS fanout are shared byte-for-byte via
//! `oracle_service::run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use oracle_service::Config;
use solana_token_info_client::TokenInfoClient;

/// Mirrors `oracle_service::Cli` — a separate type only because the default
/// config/secrets paths are baked into the clap derive.
#[derive(Parser, Debug)]
#[command(
    name = "solana-oracle-service",
    about = "Solana Pyth gateway: the shared oracle-service engine, feeds discovered from solana-token-info."
)]
struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-oracle-service/config/config.toml"
    )]
    config: PathBuf,

    /// Rendered secrets TOML carrying the Pyth API key (`[pyth] api_key`).
    #[arg(
        short = 's',
        long,
        default_value = "services/solana-oracle-service/config/secrets.toml"
    )]
    secrets: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-oracle-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;
    let secrets = oracle_service::load_secrets(&cli.secrets)?;

    // Discover the feeds to subscribe to from the SOLANA token catalog: every
    // token that carries a Pyth feed id.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!(
                "fetching catalog from solana-token-info at {}",
                cfg.token_info_url
            )
        })?;
    let feeds =
        oracle_service::resolve_feeds(snapshot.tokens.iter().filter_map(|t| t.pyth_feed().ok()))?;

    oracle_service::run(cfg, secrets, feeds).await
}
