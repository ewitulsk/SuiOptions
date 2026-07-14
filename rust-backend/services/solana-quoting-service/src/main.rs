use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use solana_quoting_service::{state, AppState, Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-quoting-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Arc::new(
        Config::load(&cfg_path)
            .await
            .with_context(|| format!("loading config from {cfg_path}"))?,
    );
    let app = Arc::new(AppState::with_global_rfq_cap(
        cfg.max_inflight_rfqs_global,
        cfg.indexer_graphql_url.clone(),
    ));

    state::spawn_reservation_evictor(Arc::clone(&app), Duration::from_millis(250));

    info!(addr = %cfg.bind_addr, protocol_id = %cfg.protocol_id, "starting ws server");
    solana_quoting_service::ws::serve(cfg.bind_addr, app, cfg).await
}
