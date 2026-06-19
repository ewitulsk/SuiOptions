//! oracle-service boot: discover feeds from token-info, open the one Pyth SSE
//! subscription (authenticated), drain it into the cache + fanout, and serve
//! REST + WS. See `lib.rs` for the architecture overview.

use std::collections::HashSet;
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
use tracing::info;

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
    let mut seen: HashSet<PriceFeedId> = HashSet::new();
    let mut feeds: Vec<PriceFeedId> = Vec::new();
    for token in &snapshot.tokens {
        if let Ok(feed) = token.pyth_feed() {
            if seen.insert(feed) {
                feeds.push(feed);
            }
        }
    }
    if feeds.is_empty() {
        anyhow::bail!("token-info catalog has no tokens with a pyth_feed_id");
    }
    info!(
        environment = %cfg.environment,
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

    let state = Arc::new(AppState {
        price_cache,
        benchmark_vol,
        fanout: fanout_tx,
        feeds,
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
