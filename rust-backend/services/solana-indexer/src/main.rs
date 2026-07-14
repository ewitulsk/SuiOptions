//! solana-indexer binary.
//!
//! Wires up:
//!   1. LaserStream subscription → per-slot batches → Postgres (worker).
//!   2. GraphQL query API for consumers.
//!
//! Run with `cargo run` from this crate's directory. Honors `RUST_LOG`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info};

use solana_indexer::decode::ProgramSet;
use solana_indexer::events::{Program, Pubkey};
use solana_indexer::worker::IngestOptions;
use solana_indexer::{establish_pool, run_migrations, Cli, Config, ProgressState, Repo, Secrets};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-indexer");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // The Helius key is required — unlike the Sui indexer's optional RPC
    // override there's no public fallback for a LaserStream subscription.
    let secrets_path = cli
        .secrets
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--secrets is required (helius api_key)"))?;
    let secrets = Secrets::load(secrets_path)
        .with_context(|| format!("loading secrets from {}", secrets_path.display()))?;

    let programs = ProgramSet::new([
        (parse_program(&cfg.programs.options_core)?, Program::Core),
        (parse_program(&cfg.programs.auction_venue)?, Program::Venue),
        (parse_program(&cfg.programs.options_vault)?, Program::Vault),
    ]);
    let program_ids = vec![
        cfg.programs.options_core.clone(),
        cfg.programs.auction_venue.clone(),
        cfg.programs.options_vault.clone(),
    ];

    // Postgres pool + embedded migrations before anything else touches it.
    let pool =
        Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size).context("establish_pool")?);
    run_migrations(&pool).context("run_migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "postgres pool ready");

    // r2d2 pool gauges: sample idle/in-use counts every 15s.
    {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            loop {
                ticker.tick().await;
                let state = pool.state();
                let idle = f64::from(state.idle_connections);
                metrics::gauge!("solana_indexer_db_pool_connections", "state" => "idle").set(idle);
                metrics::gauge!("solana_indexer_db_pool_connections", "state" => "in_use")
                    .set(f64::from(state.connections) - idle);
            }
        });
    }

    // Resume point. Replaying from the finalized watermark (not last_slot)
    // re-validates every provisional slot after a restart — replayed
    // events dedup on (signature, inner_ix_index).
    let progress_row = repo.load_progress().context("load_progress")?;
    let (from_slot, initial_provisional, start, current, finalized) = match &progress_row {
        Some(p) => {
            let finalized = p.finalized_slot.max(0) as u64;
            // Resume from the watermark so replay re-validates provisional
            // slots. A row with no finalized tick yet (crashed within
            // seconds of first start) falls back to last_slot — fromSlot=1
            // would be far outside LaserStream's ~24h replay window.
            let resume = if finalized > 0 {
                finalized + 1
            } else {
                (p.last_slot.max(0) as u64) + 1
            };
            let provisional = repo
                .provisional_slots(p.finalized_slot)
                .context("loading provisional slots")?;
            info!(
                resume,
                last_slot = p.last_slot,
                finalized,
                provisional = provisional.len(),
                "resuming from persisted progress"
            );
            (
                Some(resume),
                provisional,
                finalized,
                p.last_slot.max(0) as u64,
                finalized,
            )
        }
        None => match cfg.start_slot {
            Some(s) => {
                info!(start_slot = s, "fresh database, starting from pinned slot");
                (Some(s), vec![], s, s, 0)
            }
            None => {
                info!("fresh database, tailing from the stream tip");
                (None, vec![], 0, 0, 0)
            }
        },
    };

    let progress_state = Arc::new(ProgressState::new(start, current, finalized));

    observability::ops::spawn(cfg.health_addr);

    // GraphQL query API.
    let graphql_addr = cfg.graphql_addr;
    let graphql_repo = repo.clone();
    let graphql_progress = Arc::clone(&progress_state);
    let graphql_origins = cfg.allowed_origins.clone();
    let graphql_playground = cfg.expose_playground;
    let graphql_handle = tokio::spawn(async move {
        if let Err(e) = solana_indexer::graphql::serve(
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

    // The ingestion worker.
    let opts = IngestOptions {
        endpoint: cfg.laserstream_endpoint.clone(),
        api_key: secrets.helius.api_key.clone(),
        programs,
        program_ids,
        from_slot,
        initial_provisional,
    };
    let worker_handle = tokio::spawn(solana_indexer::worker::run(
        opts,
        repo.clone(),
        Arc::clone(&progress_state),
    ));

    info!(
        cluster = %cfg.cluster,
        graphql = %cfg.graphql_addr,
        health = %cfg.health_addr,
        "solana-indexer running"
    );

    tokio::select! {
        res = worker_handle => {
            match res {
                Ok(Ok(())) => info!("ingestion finished"),
                Ok(Err(e)) => {
                    error!(alert_id = "solana-indexer-ingestion-died", error = %e, "ingestion failed");
                    return Err(e);
                }
                Err(e) => error!(error = %e, "ingestion join failed"),
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

fn parse_program(id: &str) -> Result<Pubkey> {
    Pubkey::from_base58(id).with_context(|| format!("parsing program id {id:?}"))
}
