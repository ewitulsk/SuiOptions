use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use api_service::{catalog::TokenCatalog, router, AppState, Cli, Config};
use shared::deployments::Deployments;

#[tokio::main]
async fn main() -> Result<()> {
    shared::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let deployments = Deployments::load(&cfg.deployments_path).with_context(|| {
        format!("loading deployments from {}", cfg.deployments_path.display())
    })?;
    let catalog = TokenCatalog::from_deployments(&deployments, &cfg.network)
        .with_context(|| format!("building token catalog for network {}", cfg.network))?;

    let state = Arc::new(AppState::new(catalog));

    let url = cfg.indexer_url.clone();
    let state_for_indexer = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(e) = shared::indexer_client::run(url, state_for_indexer).await {
            tracing::error!(error = %e, "indexer subscriber exited");
        }
    });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
