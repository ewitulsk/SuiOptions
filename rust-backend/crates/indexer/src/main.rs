//! Indexer binary.
//!
//! Wires up:
//!   1. Sui checkpoint stream → `ProtocolEventWorker` (via
//!      `setup_single_workflow` from sui-data-ingestion-core).
//!   2. WS fanout server for the quoting service.
//!
//! Run with `CONFIG_PATH=config/testnet.toml cargo run -p indexer`. Honors
//! `RUST_LOG` for filtering.

use std::env;
use std::sync::Arc;

use anyhow::{Context, Result};
use sui_data_ingestion_core::setup_single_workflow;
use sui_sdk::SuiClientBuilder;
use tracing::{error, info};

use indexer::{Config, ProtocolEventWorker, Store};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg_path = env::var("CONFIG_PATH").unwrap_or_else(|_| "config/testnet.toml".into());
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path)
        .with_context(|| format!("loading config from {cfg_path}"))?;

    // Resolve the deployed package id from deployments.json so a redeploy
    // doesn't need an indexer config edit.
    let package_id = cfg.resolve_package_id().with_context(|| {
        format!(
            "resolving package_id from {} (network={})",
            cfg.deployments_path.display(),
            cfg.network
        )
    })?;
    info!(
        network = %cfg.network,
        package_id = %package_id,
        deployments = %cfg.deployments_path.display(),
        "resolved package id from deployments"
    );

    let store = Arc::new(Store::new(1024));

    // Resolve the starting checkpoint. If the operator didn't pin one, ask
    // a fullnode for the latest checkpoint and tail from there. The lookup
    // is one-shot (only at boot); we don't keep the RPC client around.
    let start_checkpoint = match cfg.start_checkpoint {
        Some(s) => {
            info!(start_checkpoint = s, "starting from pinned checkpoint");
            s
        }
        None => {
            let rpc = cfg.resolve_rpc_url()?;
            info!(rpc = %rpc, "no start_checkpoint pinned; querying tip");
            let client = SuiClientBuilder::default()
                .build(&rpc)
                .await
                .with_context(|| format!("connecting to {rpc}"))?;
            let latest = client
                .read_api()
                .get_latest_checkpoint_sequence_number()
                .await
                .context("querying latest checkpoint")?;
            info!(start_checkpoint = latest, "tailing from current tip");
            latest
        }
    };

    // Sui checkpoint ingestion. `setup_single_workflow` returns
    // `(ExecutorProgress future, termination Sender)` — we drive the future
    // alongside the fanout.
    let worker = ProtocolEventWorker::new(Arc::clone(&store), &package_id);
    let (executor, _term_sender) = setup_single_workflow(
        worker,
        cfg.remote_store_url.clone(),
        start_checkpoint,
        cfg.concurrency,
        None,
    )
    .await
    .context("setup_single_workflow")?;

    let fanout_store = Arc::clone(&store);
    let fanout_addr = cfg.fanout_addr;
    let heartbeat = cfg.heartbeat_interval();
    let fanout_handle = tokio::spawn(async move {
        if let Err(e) = indexer::fanout::serve(fanout_addr, fanout_store, heartbeat).await {
            error!(error = %e, "fanout server exited");
        }
    });

    info!(addr = %cfg.fanout_addr, "indexer fanout listening");
    info!(
        remote = %cfg.remote_store_url,
        from = start_checkpoint,
        concurrency = cfg.concurrency,
        "ingestion workflow running"
    );

    tokio::select! {
        res = executor => {
            match res {
                Ok(_) => info!("ingestion finished"),
                Err(e) => error!(error = %e, "ingestion failed"),
            }
        }
        res = fanout_handle => {
            match res {
                Ok(_) => info!("fanout finished"),
                Err(e) => error!(error = %e, "fanout join failed"),
            }
        }
    }
    Ok(())
}
