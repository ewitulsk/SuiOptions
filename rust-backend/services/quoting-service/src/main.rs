use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use quoting_service::{state, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Arc::new(
        Config::load(&cfg_path)
            .await
            .with_context(|| format!("loading config from {cfg_path}"))?,
    );
    // Verified-orgs allowlist: fail-closed at boot, then polled in the
    // background. RFQs for unverified orgs' buckets are refused.
    let verified_orgs = token_info_client::VerifiedOrgsWatcher::start(
        token_info_client::TokenInfoClient::new(&cfg.token_info_url),
        Duration::from_secs(30),
    )
    .await
    .context("loading verified-orgs allowlist from token-info")?;

    let app = Arc::new(AppState::with_global_rfq_cap(
        cfg.max_inflight_rfqs_global,
        cfg.indexer_graphql_url.clone(),
        verified_orgs,
    ));

    state::spawn_reservation_evictor(Arc::clone(&app), Duration::from_millis(250));

    info!(addr = %cfg.bind_addr, "starting ws server");
    quoting_service::ws::serve(cfg.bind_addr, app, cfg).await
}
