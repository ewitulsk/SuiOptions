use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use api_service::{catalog::TokenCatalog, router, AppState, Cli, Config};
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("api-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // Fetch the supported-token catalog from token-info. Hard cutover: if
    // token-info is unreachable after the retry window we crash (no
    // deployments.json fallback).
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cfg.token_info_url))?;
    let catalog = TokenCatalog::from_tokens(snapshot.tokens());

    let state = Arc::new(AppState::new(
        catalog,
        cfg.indexer_graphql_url.clone(),
        cfg.derived_metrics_url.clone(),
        cfg.sui_rpc_url.clone(),
        cfg.price_charting_url.clone(),
    ));

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
