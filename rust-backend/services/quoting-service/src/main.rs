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
    let app = Arc::new(AppState::with_global_rfq_cap(cfg.max_inflight_rfqs_global));

    state::spawn_reservation_evictor(Arc::clone(&app), Duration::from_millis(250));

    let url = cfg.indexer_url.clone();
    let app_for_indexer = Arc::clone(&app);
    tokio::spawn(async move {
        if let Err(e) = indexer_client::run(url, app_for_indexer).await {
            tracing::error!(error = %e, "indexer subscriber exited");
        }
    });

    info!(addr = %cfg.bind_addr, "starting ws server");
    quoting_service::ws::serve(cfg.bind_addr, app, cfg).await
}
