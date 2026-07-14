//! solana-price-charting binary.
//!
//! Boot order mirrors the house pattern: logging → config (`${VAR}`
//! expansion) → DB pool + embedded migrations → token-info snapshot (hard
//! dependency; decimals + Pyth feed ids for the apy sampler) → apy sampler →
//! axum serve. That's the whole boot path: no watcher and no mid sampler are
//! spawned because there is no Solana order-book source yet — the trade/mid
//! tables simply stay empty and the API serves what empty tables produce.
//! No chain reads either, so no RPC endpoint or secrets mount.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use solana_indexer_graphql::IndexerClient;
use solana_price_charting::config::Config;
use solana_price_charting::db::{establish_pool, repo::Repo, run_migrations};
use solana_price_charting::state::AppState;
use solana_price_charting::{apy_sampler, router, Cli};
use solana_token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-price-charting");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // The token catalog is the apy sampler's source for decimals + Pyth feed
    // ids; without it no vault can be priced, so absence is fatal (hard
    // cutover, same as every solana-token-info consumer).
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching snapshot from {}", cfg.token_info_url))?;

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running charts DB migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "charts DB ready (migrations applied)");

    let state = Arc::new(AppState::new(repo));

    // Vault-APY sampler: reads vaults/rounds/auctions and the realized series
    // from solana-indexer, and spot + realized vol from solana-oracle-service
    // (the single Pyth gateway). The sampler degrades per-tick if the oracle
    // is down — no boot gate, so chart serving stays up.
    apy_sampler::spawn(apy_sampler::ApySamplerParams {
        state: Arc::clone(&state),
        indexer: IndexerClient::new(cfg.indexer_graphql_url.clone()),
        oracle: oracle_client::OracleClient::new(&cfg.oracle_url),
        snapshot,
        tick_interval: Duration::from_secs(cfg.apy_tick_secs.max(1)),
        pyth: cfg.pyth.clone(),
        model: cfg.model.clone(),
    });
    info!(
        environment = %cfg.environment,
        indexer = %cfg.indexer_graphql_url,
        oracle = %cfg.oracle_url,
        apy_tick_secs = cfg.apy_tick_secs,
        "apy sampler running (no order-book ingestion: trade/mid tables stay empty)"
    );

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
