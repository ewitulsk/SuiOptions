//! oracle-service (SO-254).
//!
//! The single internal holder of the one Pyth Hermes SSE subscription plus all
//! Pyth caching. It:
//!   - subscribes once to Hermes (live prices → `PriceCache`),
//!   - re-broadcasts every price update over an internal WS fanout (`/ws`),
//!   - serves the latest prices over REST (`/prices`, `/prices/:feed`,
//!     `/snapshot`),
//!   - serves cached + paced realized vol from Benchmarks (`/vol/realized`,
//!     shared `BenchmarkVol` across all callers).
//!
//! Every other service reads through `oracle-client` instead of talking to Pyth
//! directly, so the one external connection and the Pyth API key live here. The
//! keeper keeps a separate direct Hermes path only for the on-chain VAA.

pub mod config;
pub mod fanout;
pub mod router;
pub mod state;

pub use config::Config;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pyth_client::{BenchmarkVol, PriceCache, PriceFeedId};
use state::AppState;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "oracle-service",
    about = "Single internal Pyth gateway: one SSE subscription, REST + WS fanout, cached realized vol."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/oracle-service/config/config.toml")]
    pub config: PathBuf,

    /// Rendered secrets TOML carrying the Pyth API key (`[pyth] api_key`).
    #[arg(
        short = 's',
        long,
        default_value = "services/oracle-service/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "oracle-service",
    cargo_pkg   = "oracle-service",
    working_dir = ".",
    description = "Single internal Pyth gateway: one SSE subscription, REST + WS fanout, cached realized vol.",
    cli         = crate::Cli,
}

const FANOUT_DEPTH: usize = 1024;

/// Load the rendered secrets TOML. The Pyth API key is optional (anonymous =
/// rate-limited but functional), so a missing secrets file must NOT block boot
/// — render-secrets.sh only writes the file when the AWS secret exists. Absent
/// file → no key.
pub fn load_secrets(path: &Path) -> Result<runtime_config::Secrets> {
    if path.exists() {
        runtime_config::Secrets::load(path)
            .with_context(|| format!("loading secrets {}", path.display()))
    } else {
        tracing::warn!(
            path = %path.display(),
            "no secrets file; running Pyth on the anonymous (rate-limited) tier"
        );
        Ok(runtime_config::Secrets::default())
    }
}

/// Dedupe the Pyth feed ids discovered from a token catalog (multiple tokens
/// can't share a feed, but be defensive), preserving catalog order. Errors if
/// the catalog yielded no feeds — nothing to subscribe to is a boot failure.
pub fn resolve_feeds(discovered: impl IntoIterator<Item = PriceFeedId>) -> Result<Vec<PriceFeedId>> {
    let mut seen: HashSet<PriceFeedId> = HashSet::new();
    let mut feeds: Vec<PriceFeedId> = Vec::new();
    for feed in discovered {
        if seen.insert(feed) {
            feeds.push(feed);
        }
    }
    if feeds.is_empty() {
        anyhow::bail!("token catalog has no tokens with a pyth_feed_id");
    }
    Ok(feeds)
}

/// Boot the service with an already-resolved feed list: open the one Pyth SSE
/// subscription (authenticated), drain it into the cache + fanout, and serve
/// REST + WS. Feed discovery stays in each binary's `main` — the Sui and
/// Solana deployments differ only in which token-info catalog they discover
/// feeds from.
pub async fn run(
    cfg: Config,
    secrets: runtime_config::Secrets,
    feeds: Vec<PriceFeedId>,
) -> Result<()> {
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
    axum::serve(listener, router::router(state))
        .await
        .context("serving oracle-service")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED_A: &str = "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";
    const FEED_B: &str = "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a";

    #[test]
    fn resolve_feeds_dedupes_preserving_order() {
        let a = PriceFeedId::from_hex(FEED_A).unwrap();
        let b = PriceFeedId::from_hex(FEED_B).unwrap();
        let feeds = resolve_feeds([a, b, a]).unwrap();
        assert_eq!(feeds, vec![a, b]);
    }

    #[test]
    fn resolve_feeds_errors_on_empty_catalog() {
        assert!(resolve_feeds(std::iter::empty::<PriceFeedId>()).is_err());
    }
}
