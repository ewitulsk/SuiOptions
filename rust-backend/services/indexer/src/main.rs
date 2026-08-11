//! Indexer binary.
//!
//! Wires up:
//!   1. Sui checkpoint stream → `ProtocolEventWorker` (via
//!      `setup_single_workflow` from sui-data-ingestion-core).
//!   2. GraphQL query API for consumers.
//!
//! Run with `cargo run -p indexer`. Set `CONFIG_PATH` to override the
//! default `services/indexer/config/config.toml`. Honors `RUST_LOG`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use sui_data_ingestion_core::setup_single_workflow;
use sui_tx::chain::ChainClient;
use token_info_client::TokenInfoClient;
use tracing::{error, info, warn};

use indexer::{
    establish_pool, run_migrations, Cli, Config, ProgressState, ProtocolEventWorker, Repo, Store,
};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init_with("indexer", &["sui_data_ingestion_core=off"]);

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path)
        .with_context(|| format!("loading config from {cfg_path}"))?;

    // Resolve the gRPC endpoint: prefer the shared `[sui] grpc_url`
    // override from the optional secrets file (rendered by render-secrets.sh)
    // over the config / public default. Optional by design — a missing or
    // unreadable secrets file degrades to cfg.resolve_grpc_url() so the indexer
    // never crash-loops when the secret isn't rendered.
    let grpc_url = match load_grpc_override(cli.secrets.as_deref()) {
        Some(u) => u,
        None => cfg.resolve_grpc_url()?,
    };
    info!(rpc = %redact_rpc(&grpc_url), "resolved Sui gRPC endpoint");

    // Resolve the deployed package id from the token-info service so a
    // redeploy doesn't need an indexer config edit. Hard cutover: if
    // token-info is unreachable after the retry window we crash (no
    // deployments.json fallback). Do this before the DB pool so an outage
    // fails fast.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!("fetching package_id from token-info at {}", cfg.token_info_url)
        })?;
    let core_package_id = snapshot.package_info.package_id.clone();
    // Four-package layout: the auction / options_rfq ids are required — a
    // deployment record missing one predates the split and can't be indexed,
    // so fail at boot rather than silently dropping their events.
    let auction_package_id = snapshot
        .auction()
        .map(|p| p.package_id.clone())
        .ok_or_else(|| anyhow::anyhow!("token-info package_info is missing the auction package id"))?;
    let rfq_package_id = snapshot
        .rfq()
        .map(|p| p.package_id.clone())
        .ok_or_else(|| anyhow::anyhow!("token-info package_info is missing the rfq package id"))?;
    // options_vault is the exception: the covered-call vault product is
    // deprecated (SO-332) and the package is no longer published, so its
    // absence is expected and its 17 event families just don't subscribe.
    let vault_package_id = snapshot.vault().map(|p| p.package_id.clone());
    // DeepBook PoolCreated events resolve to the ORIGINAL package id (SO-152).
    // Absent on networks without a DeepBook deployment — ingestion of pool
    // events is simply off there.
    let deepbook_original = snapshot
        .deepbook()
        .map(|d| d.original_package_id.clone());
    // Curated trading-vault packages (SO-282): optional — absent on
    // deployments predating the product, where their event families are
    // simply not subscribed.
    let trading_vault_package_id = snapshot.trading_vault().map(|p| p.package_id.clone());
    let deepbook_adapter_package_id = snapshot.deepbook_adapter().map(|p| p.package_id.clone());
    let options_adapter_package_id = snapshot.options_adapter().map(|p| p.package_id.clone());
    let exchange_adapter_package_id = snapshot.exchange_adapter().map(|p| p.package_id.clone());
    let equity_oracle_package_id = snapshot.equity_oracle().map(|p| p.package_id.clone());
    info!(
        network = %cfg.network,
        core_package_id = %core_package_id,
        auction_package_id = %auction_package_id,
        rfq_package_id = %rfq_package_id,
        vault_package_id = %vault_package_id.as_deref().unwrap_or("<deprecated, SO-332>"),
        deepbook_original = %deepbook_original.as_deref().unwrap_or("<none>"),
        trading_vault_package_id = %trading_vault_package_id.as_deref().unwrap_or("<none>"),
        token_info_url = %cfg.token_info_url,
        "resolved package ids from token-info"
    );

    // Stand up the DB pool and apply pending migrations before anything else
    // touches Postgres — migrations are embedded in the binary.
    let pool = Arc::new(
        establish_pool(&cfg.database_url, cfg.db_pool_size).context("establish_pool")?,
    );
    run_migrations(&pool).context("run_migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "postgres pool ready");

    // r2d2 pool gauges (SO-180): sample idle/in-use counts every 15s.
    {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            loop {
                ticker.tick().await;
                let state = pool.state();
                let idle = f64::from(state.idle_connections);
                metrics::gauge!("indexer_db_pool_connections", "state" => "idle").set(idle);
                metrics::gauge!("indexer_db_pool_connections", "state" => "in_use")
                    .set(f64::from(state.connections) - idle);
            }
        });
    }

    let store = Arc::new(Store::new());

    // Hydrate the in-memory views from Postgres. After this call the store
    // looks identical to the one we'd have built by replaying every event
    // through `Store::ingest` — `bucket()` / `account()` work immediately.
    let progress = repo.load_progress().context("load_progress")?;
    let hydrate_start = std::time::Instant::now();
    let views = repo.hydrate().context("hydrate views")?;
    let last_persisted_sequence = progress.as_ref().map(|p| p.last_sequence as u64).unwrap_or(0);
    let persisted_last_checkpoint = progress.as_ref().map(|p| p.last_checkpoint as u64);
    store.hydrate(views, last_persisted_sequence);
    metrics::gauge!("indexer_hydrate_duration_seconds")
        .set(hydrate_start.elapsed().as_secs_f64());
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
                info!("no start_checkpoint pinned; querying tip");
                let client = ChainClient::new(&grpc_url)
                    .with_context(|| format!("connecting to {}", redact_rpc(&grpc_url)))?;
                let latest = client
                    .latest_checkpoint()
                    .await
                    .context("querying latest checkpoint")?;
                info!(start_checkpoint = latest, "tailing from current tip");
                latest
            }
        }
    };

    // Progress tracking for `GET /progress` (SO-107). The bar's origin (0%) is
    // the run's true starting checkpoint: the config-pinned value if set, else
    // the boot tip we resolved above. `current` seeds from persisted progress
    // so a resume reports the right position before the next checkpoint lands.
    let origin_start = cfg.start_checkpoint.unwrap_or(start_checkpoint);
    let initial_current = persisted_last_checkpoint.unwrap_or(origin_start);
    let progress_state = Arc::new(ProgressState::new(origin_start, initial_current));

    // Background tip-poller: refresh the chain head every 10s so the bar has a
    // live 100% target. Best-effort — if no RPC resolves, /progress just omits
    // the tip (frontend shows current/rate without a percentage).
    {
        let tip_rpc = grpc_url.clone();
        let tip_state = Arc::clone(&progress_state);
        tokio::spawn(async move {
            let mut client = None;
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                if client.is_none() {
                    client = ChainClient::new(&tip_rpc).ok();
                }
                let Some(c) = client.as_ref() else { continue };
                match c.latest_checkpoint().await {
                    Ok(tip) => tip_state.set_tip(tip),
                    Err(e) => {
                        warn!(error = %e, "tip poll failed; will reconnect");
                        client = None;
                    }
                }
            }
        });
    }

    // Sui checkpoint ingestion. `setup_single_workflow` returns
    // `(ExecutorProgress future, termination Sender)` — we drive the future
    // alongside the fanout.
    let worker = ProtocolEventWorker::new(
        Arc::clone(&store),
        repo.clone(),
        indexer::event_types::PackageIds {
            core: &core_package_id,
            auction: &auction_package_id,
            rfq: &rfq_package_id,
            vault: vault_package_id.as_deref(),
            trading_vault: trading_vault_package_id.as_deref(),
            deepbook_adapter: deepbook_adapter_package_id.as_deref(),
            options_adapter: options_adapter_package_id.as_deref(),
            exchange_adapter: exchange_adapter_package_id.as_deref(),
            equity_oracle: equity_oracle_package_id.as_deref(),
        },
        deepbook_original.as_deref(),
        Arc::clone(&progress_state),
    );
    let (executor, _term_sender) = setup_single_workflow(
        worker,
        cfg.remote_store_url.clone(),
        start_checkpoint,
        cfg.concurrency,
        None,
    )
    .await
    .context("setup_single_workflow")?;

    // The indexer was already honest before SO-324: the spawn sits *after*
    // every fallible step (store, repo, package ids, `setup_single_workflow`),
    // so nothing served /health until startup had succeeded. The flip is
    // immediate here for that reason — it preserves the existing behaviour
    // rather than changing it. Only the graphql task and the ingestion select
    // follow, and neither can fail startup.
    let readiness = observability::ops::Readiness::new();
    observability::ops::spawn(cfg.health_addr, &readiness);
    readiness.ready();

    // GraphQL query API (SO-97). Consumers read protocol state just-in-time
    // from here; it's the indexer's only outbound query surface.
    let graphql_addr = cfg.graphql_addr;
    let graphql_repo = repo.clone();
    let graphql_progress = Arc::clone(&progress_state);
    let graphql_origins = cfg.allowed_origins.clone();
    let graphql_playground = cfg.expose_playground;
    let graphql_handle = tokio::spawn(async move {
        if let Err(e) = indexer::graphql::serve(
            graphql_addr,
            graphql_repo,
            graphql_progress,
            &graphql_origins,
            graphql_playground,
        )
        .await
        {
            error!(error = %e, "graphql server exited");
        }
    });

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
        res = graphql_handle => {
            match res {
                Ok(_) => info!("graphql finished"),
                Err(e) => error!(error = %e, "graphql join failed"),
            }
        }
    }
    Ok(())
}

/// Read `[sui] grpc_url` from the optional secrets file. Returns `None` (with a
/// warning) when the path is unset or the file is missing/unparseable, so the
/// caller falls back to the config / public RPC rather than crash-looping.
fn load_grpc_override(path: Option<&std::path::Path>) -> Option<String> {
    let path = path?;
    match runtime_config::Secrets::load(path) {
        Ok(s) => s.sui.grpc_url,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "secrets file unreadable; using config/public RPC");
            None
        }
    }
}

/// Strip any token-bearing path from an RPC URL for logging, keeping the host.
fn redact_rpc(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or(url)
}
