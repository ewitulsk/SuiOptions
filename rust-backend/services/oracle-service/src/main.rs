//! oracle-service boot: discover feeds from token-info, open the one Pyth SSE
//! subscription (authenticated), drain it into the cache + fanout, and serve
//! REST + WS. See `lib.rs` for the architecture overview.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use oracle_service::{config::Config, fanout, router::router, state::AppState, Cli};
use pyth_client::{BenchmarkVol, PriceCache, PriceFeedId};
use token_info_client::TokenInfoClient;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{info, warn};

const FANOUT_DEPTH: usize = 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("oracle-service");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;
    // The Pyth API key is optional (anonymous = rate-limited but functional), so
    // a missing secrets file must NOT block boot — render-secrets.sh only writes
    // oracle-service.toml when the AWS secret exists. Absent file → no key.
    let secrets = if cli.secrets.exists() {
        runtime_config::Secrets::load(&cli.secrets)
            .with_context(|| format!("loading secrets {}", cli.secrets.display()))?
    } else {
        tracing::warn!(
            path = %cli.secrets.display(),
            "no secrets file; running Pyth on the anonymous (rate-limited) tier"
        );
        runtime_config::Secrets::default()
    };

    // Discover the feeds to subscribe to from the token-info catalog: every
    // token that carries a Pyth feed id (deduped — multiple tokens can't, but
    // be defensive).
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cfg.token_info_url))?;
    // Feed discovery follows the CONFIGURED PROVIDER (SO-335): the same
    // catalog carries both providers' keys, and this is where the switch
    // takes effect for the data plane.
    let provider = cfg.oracle.provider;
    let mut seen: HashSet<PriceFeedId> = HashSet::new();
    let mut feeds: Vec<PriceFeedId> = Vec::new();
    let mut feed_by_asset: BTreeMap<String, PriceFeedId> = BTreeMap::new();
    for token in &snapshot.tokens {
        let Some(raw) = token.feed_for(provider) else {
            continue;
        };
        let Ok(feed) = PriceFeedId::from_hex(raw) else {
            warn!(
                ticker = %token.ticker,
                %provider,
                "catalog feed key is not 32-byte hex; skipping"
            );
            continue;
        };
        if seen.insert(feed) {
            feeds.push(feed);
        }
        feed_by_asset.insert(
            protocol_types::asset::canonicalize_move_type(&token.coin_type),
            feed,
        );
    }
    if feeds.is_empty() {
        anyhow::bail!(
            "token-info catalog has no tokens with a {provider} feed key —              the catalog must be seeded for a provider before it can go live"
        );
    }
    info!(
        environment = %cfg.environment,
        %provider,
        feeds = feeds.len(),
        has_pyth_key = secrets.pyth_api_key().is_some(),
        hermes = %cfg.hermes_url,
        "oracle-service starting"
    );

    // One authenticated HTTP client shared by the SSE stream and Benchmarks.
    let http = reqwest::Client::builder()
        .default_headers(pyth_client::auth_headers(secrets.pyth_api_key()))
        .build()
        .context("building pyth http client")?;

    let price_cache = PriceCache::new();
    let benchmark_vol = Arc::new(BenchmarkVol::new(http.clone(), cfg.benchmarks_url.clone()));
    let (fanout_tx, _) = broadcast::channel(FANOUT_DEPTH);
    let upstream_healthy = Arc::new(AtomicBool::new(false));

    // The single external Pyth SSE subscription → drain loop → cache + fanout.
    let rx = pyth_client::spawn_subscriber(http.clone(), cfg.hermes_url.clone(), feeds.clone());
    tokio::spawn(fanout::run(
        rx,
        price_cache.clone(),
        fanout_tx.clone(),
        upstream_healthy.clone(),
    ));

    // Adapter identity for the LIVE provider only. Absent is not fatal:
    // the data plane (spot, vol) still serves, and only the descriptor —
    // i.e. PTB composition — degrades, which is the honest failure mode
    // when a provider's adapter is not deployed on this network yet.
    let adapter = adapter_ids(&snapshot, provider);
    if adapter.is_none() {
        warn!(
            %provider,
            "no adapter package for the live provider in token-info —              /oracle/descriptor will report it unavailable and PTB              composers cannot build price legs"
        );
    }

    let state = Arc::new(AppState {
        price_cache,
        benchmark_vol,
        fanout: fanout_tx,
        feeds,
        provider,
        feed_by_asset,
        adapter,
        upstream_healthy,
    });

    let listener = TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;
    info!(addr = %cfg.bind_addr, "oracle-service listening");
    axum::serve(listener, router(state))
        .await
        .context("serving oracle-service")?;
    Ok(())
}

/// Resolve the live provider's on-chain adapter identity from token-info.
///
/// Deliberately per-provider: pairing one provider's adapter package with
/// another's feed registry would produce PTBs that abort on chain, so the
/// two ids are only ever read together.
fn adapter_ids(
    snapshot: &token_info_client::Snapshot,
    provider: protocol_types::OracleProvider,
) -> Option<oracle_service::state::AdapterIds> {
    let objects = snapshot.trading_vault_objects()?;
    let oracle_registry_id = objects.oracle_registry().ok()?;
    match provider {
        protocol_types::OracleProvider::Pyth => Some(oracle_service::state::AdapterIds {
            adapter_package_id: snapshot.oracle_pyth()?.package().ok()?,
            feed_registry_id: objects.pyth_feed_registry().ok()?,
            oracle_registry_id,
        }),
        protocol_types::OracleProvider::Switchboard => Some(oracle_service::state::AdapterIds {
            adapter_package_id: snapshot.oracle_switchboard()?.package().ok()?,
            feed_registry_id: objects.switchboard_feed_registry().ok()??,
            oracle_registry_id,
        }),
    }
}
