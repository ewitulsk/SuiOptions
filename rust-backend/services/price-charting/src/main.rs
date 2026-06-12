//! price-charting binary (SO-156).
//!
//! Boot order mirrors the house pattern: logging → config (`${VAR}`
//! expansion) → DB pool + embedded migrations → token-info snapshot (hard
//! dependency; provides DeepBook's original package id for the event
//! filter) → watcher task → axum serve.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use api_service_client::ApiServiceClient;
use price_charting::config::Config;
use price_charting::db::{establish_pool, repo::Repo, run_migrations};
use price_charting::state::AppState;
use price_charting::{mid_sampler, router, watcher, Cli};
use sui_sdk::SuiClientBuilder;
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("price-charting");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    // DeepBook's ORIGINAL package id comes from token-info (SO-151) — the
    // OrderFilled event type resolves there. Without a DeepBook deployment
    // this service has nothing to chart, so absence is fatal.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cfg.token_info_url))?;
    let deepbook = snapshot
        .deepbook()
        .context("token-info reports no DeepBook deployment for this network")?;
    let original_package = deepbook.original_package_id.clone();
    // The CURRENT (upgraded) package id — the mid sampler's dev-inspect
    // calls execute there, while events keep resolving to the original.
    let current_package = deepbook.package()?;

    let pool = Arc::new(establish_pool(&cfg.database_url, cfg.db_pool_size)?);
    run_migrations(&pool).context("running charts DB migrations")?;
    let repo = Repo::new(Arc::clone(&pool));
    info!(pool_size = cfg.db_pool_size, "charts DB ready (migrations applied)");

    let rpc = cfg.resolve_rpc_url()?;
    let sui = SuiClientBuilder::default()
        .build(&rpc)
        .await
        .with_context(|| format!("connecting to {rpc}"))?;

    let state = Arc::new(AppState::new(repo));
    watcher::spawn(watcher::WatcherParams {
        state: Arc::clone(&state),
        sui: sui.clone(),
        api: ApiServiceClient::new(&cfg.api_service_url),
        deepbook_original_package: original_package,
        discovery_interval: Duration::from_secs(cfg.discovery_interval_secs.max(5)),
        poll_interval: Duration::from_millis(cfg.poll_interval_ms.max(500)),
        ttl_hours: cfg.ttl_hours,
    });
    mid_sampler::spawn(mid_sampler::MidSamplerParams {
        state: Arc::clone(&state),
        sui,
        deepbook_package: current_package,
        sample_interval: Duration::from_secs(cfg.mid_sample_interval_secs.max(2)),
    });
    info!(
        environment = %cfg.environment,
        api_service = %cfg.api_service_url,
        rpc = %rpc,
        ttl_hours = cfg.ttl_hours,
        mid_sample_interval_secs = cfg.mid_sample_interval_secs,
        "watcher + mid sampler running"
    );

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}
