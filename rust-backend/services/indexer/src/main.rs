//! Indexer binary.
//!
//! Wires up:
//!   1. Sui checkpoint stream → `ProtocolEventWorker` (via
//!      `setup_single_workflow` from sui-data-ingestion-core).
//!   2. WS fanout server for the quoting service.
//!
//! Run with `cargo run -p indexer`. Set `CONFIG_PATH` to override the
//! default `services/indexer/config/config.toml`. Honors `RUST_LOG`.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use sui_data_ingestion_core::setup_single_workflow;
use sui_sdk::SuiClientBuilder;
use tracing::{error, info};

use indexer::{establish_pool, run_migrations, Cli, Config, ProtocolEventWorker, Repo, Store};

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init_with(&["sui_data_ingestion_core=off"]);

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
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

    // Stand up the DB pool and apply pending migrations before anything else
    // touches Postgres — migrations are embedded in the binary.
    let pool = Arc::new(
        establish_pool(&cfg.database_url, cfg.db_pool_size).context("establish_pool")?,
    );
    run_migrations(&pool).context("run_migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "postgres pool ready");

    let store = Arc::new(Store::new(cfg.recent_log_capacity));

    // Hydrate the in-memory views from Postgres. After this call the store
    // looks identical to the one we'd have built by replaying every event
    // through `Store::ingest` — `bucket()` / `account()` work immediately.
    let progress = repo.load_progress().context("load_progress")?;
    let recent_log = repo
        .recent_events(cfg.recent_log_capacity as i64)
        .context("recent_events")?;
    let views = repo.hydrate().context("hydrate views")?;
    let last_persisted_sequence = progress.as_ref().map(|p| p.last_sequence as u64).unwrap_or(0);
    store.hydrate(views, last_persisted_sequence, recent_log);
    info!(
        accounts = store.account_count(),
        buckets = store.bucket_count(),
        positions = store.position_count(),
        last_sequence = last_persisted_sequence,
        "hydrated in-memory views from postgres"
    );

    // Resolve the starting checkpoint. Priority:
    //   1. Persisted progress (resume where we left off).
    //   2. Pinned config value.
    //   3. Current tip via RPC.
    let start_checkpoint = if let Some(p) = progress {
        let resume = (p.last_checkpoint as u64) + 1;
        info!(resume, "resuming from persisted progress");
        resume
    } else {
        match cfg.start_checkpoint {
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
        }
    };

    // Sui checkpoint ingestion. `setup_single_workflow` returns
    // `(ExecutorProgress future, termination Sender)` — we drive the future
    // alongside the fanout.
    let worker = ProtocolEventWorker::new(Arc::clone(&store), repo.clone(), &package_id);
    let (executor, _term_sender) = setup_single_workflow(
        worker,
        cfg.remote_store_url.clone(),
        start_checkpoint,
        cfg.concurrency,
        None,
    )
    .await
    .context("setup_single_workflow")?;

    runtime_config::health::spawn(cfg.health_addr);

    let fanout_store = Arc::clone(&store);
    let fanout_addr = cfg.fanout_addr;
    let heartbeat = cfg.heartbeat_interval();
    let fanout_handle = tokio::spawn(async move {
        if let Err(e) = indexer::fanout::serve(fanout_addr, fanout_store, heartbeat).await {
            error!(error = %e, "fanout server exited");
        }
    });

    // GraphQL query API (SO-97). Reads the same Postgres views the worker
    // writes; independent of the WS fanout.
    let graphql_addr = cfg.graphql_addr;
    let graphql_repo = repo.clone();
    let graphql_handle = tokio::spawn(async move {
        if let Err(e) = indexer::graphql::serve(graphql_addr, graphql_repo).await {
            error!(error = %e, "graphql server exited");
        }
    });

    info!(addr = %cfg.fanout_addr, "indexer fanout listening");
    info!(addr = %cfg.graphql_addr, "indexer graphql listening");
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
        res = graphql_handle => {
            match res {
                Ok(_) => info!("graphql finished"),
                Err(e) => error!(error = %e, "graphql join failed"),
            }
        }
    }
    Ok(())
}
