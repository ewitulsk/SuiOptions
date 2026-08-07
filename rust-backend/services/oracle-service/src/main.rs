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

    // Discover the feeds to subscribe to from the token-info catalog.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cfg.token_info_url))?;
    // Two feed maps, deliberately split (SO-346, reworked in SO-353):
    // - The DATA PLANE always PUBLISHES under the PYTH ids — WS consumers
    //   (mm-bot's per-RFQ hot path) key their caches by these ids, so the
    //   ids are cache keys, not a Pyth dependency. The SOURCE follows the
    //   provider: Hermes SSE on pyth, our crossbar's /v2/simulate poller
    //   on switchboard (hermes-beta went dark 2026-08-04 and starved
    //   quoting for 41h — see data_plane.rs).
    // - The DESCRIPTOR (and /oracle/legs) follow the CONFIGURED PROVIDER
    //   (SO-335): that is the switch's real job — which adapter's price
    //   legs PTB composers build.
    let provider = cfg.oracle.provider;
    let mut seen: HashSet<PriceFeedId> = HashSet::new();
    let mut feeds: Vec<PriceFeedId> = Vec::new();
    let mut feed_by_asset: BTreeMap<String, PriceFeedId> = BTreeMap::new();
    let mut descriptor_feeds: BTreeMap<String, PriceFeedId> = BTreeMap::new();
    // switchboard hash → the pyth id the data plane publishes under
    // (SO-353; see data_plane.rs for why the pyth id stays the key).
    let mut data_plane_alias: BTreeMap<String, PriceFeedId> = BTreeMap::new();
    for token in &snapshot.tokens {
        let asset = protocol_types::asset::canonicalize_move_type(&token.coin_type);
        let mut pyth_feed = None;
        if let Some(raw) = token.pyth_feed_id.as_deref() {
            if let Ok(feed) = PriceFeedId::from_hex(raw) {
                if seen.insert(feed) {
                    feeds.push(feed);
                }
                feed_by_asset.insert(asset.clone(), feed);
                pyth_feed = Some(feed);
            } else {
                warn!(ticker = %token.ticker, "catalog pyth feed id is not 32-byte hex; skipping");
            }
        }
        if let Some(raw) = token.feed_for(provider) {
            match PriceFeedId::from_hex(raw) {
                Ok(feed) => {
                    descriptor_feeds.insert(asset, feed);
                }
                Err(_) => warn!(
                    ticker = %token.ticker,
                    %provider,
                    "catalog feed key is not 32-byte hex; skipping"
                ),
            }
        }
        if provider == protocol_types::OracleProvider::Switchboard {
            match (pyth_feed, token.switchboard_feed_id.as_deref()) {
                (Some(feed), Some(raw)) => {
                    let h = raw.trim();
                    let h = h
                        .strip_prefix("0x")
                        .or_else(|| h.strip_prefix("0X"))
                        .unwrap_or(h)
                        .to_ascii_lowercase();
                    data_plane_alias.insert(h, feed);
                }
                (Some(_), None) => warn!(
                    ticker = %token.ticker,
                    "no switchboard feed hash — the data plane cannot price this token and its consumers will see it stale"
                ),
                _ => {}
            }
        }
    }
    if feeds.is_empty() {
        anyhow::bail!(
            "token-info catalog has no tokens with a pyth feed id —              the data plane has nothing to subscribe to"
        );
    }
    if descriptor_feeds.is_empty() {
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

    // One upstream → drain loop → cache + fanout. The source follows the
    // provider (SO-353); everything downstream of the channel is shared.
    let rx = match provider {
        protocol_types::OracleProvider::Pyth => {
            pyth_client::spawn_subscriber(http.clone(), cfg.hermes_url.clone(), feeds.clone())
        }
        protocol_types::OracleProvider::Switchboard => {
            // Data plane and signed-quote path may point at different
            // crossbar instances — see `data_plane_crossbar_url` docs.
            let crossbar_url = cfg
                .oracle
                .data_plane_crossbar_url
                .clone()
                .or_else(|| cfg.oracle.crossbar_url.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider=switchboard requires [oracle] data_plane_crossbar_url \
                         (or crossbar_url)"
                    )
                })?;
            if data_plane_alias.is_empty() {
                anyhow::bail!(
                    "provider=switchboard but no catalog token carries BOTH a pyth feed id \
                     (the cache key) and a switchboard feed hash — the data plane would \
                     serve nothing"
                );
            }
            oracle_service::data_plane::spawn_crossbar_poller(
                switchboard_client::CrossbarClient::new(&crossbar_url, None),
                data_plane_alias,
                feeds.len(),
            )
        }
    };
    tokio::spawn(fanout::run(
        rx,
        price_cache.clone(),
        fanout_tx.clone(),
        upstream_healthy.clone(),
    ));
    // Delivery watchdog (SO-354): pages when the fanout goes quiet,
    // whichever upstream is live.
    fanout::spawn_stale_watchdog(fanout_tx.subscribe());

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

    // Off-chain payload source for `GET /oracle/legs` (SO-346). Pyth
    // reuses the authenticated Hermes client; Switchboard requires the
    // full Crossbar config — a switchboard deployment without it cannot
    // build price legs, so that is a loud boot failure, not a silent
    // degrade (the "works until you flip" trap this seam exists to kill).
    let legs = match provider {
        protocol_types::OracleProvider::Pyth => oracle_service::state::LegsBackend::Pyth {
            http: http.clone(),
            hermes_url: cfg.hermes_url.clone(),
        },
        protocol_types::OracleProvider::Switchboard => {
            switchboard_legs_backend(&cfg.oracle).await?
        }
    };

    let state = Arc::new(AppState {
        price_cache,
        benchmark_vol,
        fanout: fanout_tx,
        feeds,
        provider,
        feed_by_asset,
        descriptor_feeds,
        adapter,
        legs,
        upstream_healthy,
    });

    let listener = TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;
    info!(addr = %cfg.bind_addr, "oracle-service listening");
    axum::serve(listener, router(state, &cfg.allowed_origins)?)
        .await
        .context("serving oracle-service")?;
    Ok(())
}

/// Build the Switchboard legs backend: require the full Crossbar config,
/// wait (bounded) for Crossbar itself — it boots in the same compose wave
/// — then resolve the oracle-pubkey → Sui-object map once.
async fn switchboard_legs_backend(
    oracle: &oracle_service::config::OracleConfig,
) -> Result<oracle_service::state::LegsBackend> {
    let require = |v: &Option<String>, k: &str| -> Result<String> {
        v.clone()
            .ok_or_else(|| anyhow::anyhow!("provider=switchboard requires [oracle] {k}"))
    };
    let crossbar_url = require(&oracle.crossbar_url, "crossbar_url")?;
    let queue_key = require(&oracle.switchboard_queue_key, "switchboard_queue_key")?;
    let switchboard_package_id =
        require(&oracle.switchboard_package_id, "switchboard_package_id")?;
    let sui_rpc_url = require(&oracle.sui_rpc_url, "sui_rpc_url")?;
    let queue_id = require(&oracle.switchboard_queue_id, "switchboard_queue_id")?
        .parse::<sui_types::base_types::ObjectID>()
        .context("parsing [oracle] switchboard_queue_id")?;

    let crossbar =
        switchboard_client::CrossbarClient::new(&crossbar_url, oracle.crossbar_network.clone());
    let mut last_err = None;
    for attempt in 1..=30u32 {
        match crossbar.health().await {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                warn!(%crossbar_url, attempt, error = %format!("{e:#}"), "crossbar not ready; retrying");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if let Some(e) = last_err {
        return Err(e.context(format!("crossbar at {crossbar_url} unreachable")));
    }
    // The signer map comes from the CHAIN, not crossbar — see
    // switchboard_client::oracles_from_queue for why.
    let oracles = switchboard_client::oracles_from_queue(&sui_rpc_url, queue_id)
        .await
        .context("resolving the queue's registered oracles from chain")?;
    info!(oracles = oracles.len(), "switchboard legs backend ready");
    Ok(oracle_service::state::LegsBackend::Switchboard {
        crossbar,
        oracles: tokio::sync::RwLock::new(oracles),
        sui_rpc_url,
        queue_id,
        queue_key,
        switchboard_package_id,
    })
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
