use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use api_service::{router, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    shared::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;
    let state = Arc::new(AppState::new());

    let url = cfg.indexer_url.clone();
    let state_for_indexer = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(e) = shared::indexer_client::run(url, state_for_indexer).await {
            tracing::error!(error = %e, "indexer subscriber exited");
        }
    });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
