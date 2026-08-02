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
use sui_tx::chain::ChainClient;
use sui_tx::events::EventClient;
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

    // Prefer the shared `[sui] grpc_url`/`graphql_url` overrides from the
    // optional secrets file (rendered by render-secrets.sh) over the config /
    // public defaults. The watcher's poll loop is a heavy RPC consumer, so
    // this is the main reason price-charting carries a secrets mount.
    // Optional: a missing/unreadable file degrades to the config values.
    let overrides = load_endpoint_overrides(cli.secrets.as_deref());
    let grpc = match overrides.as_ref().and_then(|s| s.0.clone()) {
        Some(u) => u,
        None => cfg.resolve_grpc_url()?,
    };
    let graphql = match overrides.as_ref().and_then(|s| s.1.clone()) {
        Some(u) => u,
        None => cfg.resolve_graphql_url()?,
    };
    info!(rpc = %redact_rpc(&grpc), "resolved Sui gRPC endpoint");
    let sui = ChainClient::new(&grpc)
        .with_context(|| format!("connecting to {}", redact_rpc(&grpc)))?;
    let events = EventClient::new(&graphql);

    let state = Arc::new(AppState::new(repo));
    watcher::spawn(watcher::WatcherParams {
        state: Arc::clone(&state),
        sui: sui.clone(),
        events,
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

    // The vault-APY sampler is NOT spawned: it sampled covered-call vaults,
    // and that product is deprecated (SO-332). With no vaults on chain it
    // would poll the indexer forever for an empty list. `apy_sampler` and the
    // `vault_{predicted,realized}_apy` hypertables stay in-tree — the tables
    // still hold the historical series and are queryable directly.
    info!(
        environment = %cfg.environment,
        api_service = %cfg.api_service_url,
        rpc = %redact_rpc(&grpc),
        ttl_hours = cfg.ttl_hours,
        mid_sample_interval_secs = cfg.mid_sample_interval_secs,
        "watcher + mid sampler running"
    );

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}

/// Read `[sui] grpc_url` / `graphql_url` from the optional secrets file.
/// Returns `None` (with a warning) when the path is unset or the file is
/// missing/unparseable, so the caller falls back to the config / public
/// endpoints rather than crash-looping.
#[allow(clippy::type_complexity)]
fn load_endpoint_overrides(
    path: Option<&std::path::Path>,
) -> Option<(Option<String>, Option<String>)> {
    let path = path?;
    match runtime_config::Secrets::load(path) {
        Ok(s) => Some((s.sui.grpc_url, s.sui.graphql_url)),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "secrets file unreadable; using config/public endpoints");
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
