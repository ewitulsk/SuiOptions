use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use api_service::{catalog::TokenCatalog, router, AppState, Cli, Config};
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // Fetch the supported-token catalog from token-info. Hard cutover: if
    // token-info is unreachable after the retry window we crash (no
    // deployments.json fallback).
    let token_info = TokenInfoClient::new(&cfg.token_info_url);
    let snapshot = token_info
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cfg.token_info_url))?;
    let catalog = TokenCatalog::from_tokens(snapshot.tokens());

    // Verified-orgs allowlist: fail-closed at boot, then polled in the
    // background (the list changes via human admin action; ~30s staleness
    // is fine). Refresh failures keep the last good set.
    let verified_orgs =
        token_info_client::VerifiedOrgsWatcher::start(token_info, Duration::from_secs(30))
            .await
            .context("loading verified-orgs allowlist from token-info")?;

    let state = Arc::new(AppState::new(
        catalog,
        cfg.indexer_graphql_url.clone(),
        verified_orgs,
    ));

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
