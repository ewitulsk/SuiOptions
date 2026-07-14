//! oracle-service boot: discover feeds from token-info, then hand off to the
//! shared engine (`oracle_service::run`) — the one Pyth SSE subscription,
//! cache + fanout, REST + WS. See `lib.rs` for the architecture overview.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use oracle_service::{config::Config, Cli};
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("oracle-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;
    let secrets = oracle_service::load_secrets(&cli.secrets)?;

    // Discover the feeds to subscribe to from the token-info catalog: every
    // token that carries a Pyth feed id.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cfg.token_info_url))?;
    let feeds =
        oracle_service::resolve_feeds(snapshot.tokens.iter().filter_map(|t| t.pyth_feed().ok()))?;

    oracle_service::run(cfg, secrets, feeds).await
}
