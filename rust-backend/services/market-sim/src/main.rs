//! market-sim boot: config + secrets, ops server, token-info catalog,
//! oracle-service price cache, then the spot-band loop.
//!
//! Gate posture: this is a simulator — when any gate fails (disabled,
//! non-testnet, no faucets, no DeepBook deployment) it WARNS and parks
//! with /health green instead of exiting. A crash-looping sim would fail
//! the deploy health gate and roll back the whole planned set; parking
//! keeps it inert until the next redeploy fixes the input.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use market_sim::liquidity::FaucetMinter;
use market_sim::{sim, Cli, Config};
use sui_tx::tx::deepbook::DeepBookHandles;
use sui_tx::Network;
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("market-sim");

    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)
        .with_context(|| format!("loading config {}", cli.config.display()))?;
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let readiness = observability::ops::Readiness::new();
    observability::ops::spawn(cfg.health_addr, &readiness);

    if !cfg.enabled {
        return park("disabled by config", &readiness).await;
    }
    if cfg.network != Network::Testnet {
        return park("testnet only", &readiness).await;
    }

    // Token catalog + DeepBook deployment from token-info. Hard cutover: if
    // token-info is unreachable after the retry window we crash (same
    // posture as every other consumer — a missing catalog is an env fault,
    // not a sim condition).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    if snapshot.test_tokens().is_err() {
        return park("token catalog has no faucets", &readiness).await;
    }
    let Some(db) = snapshot.deepbook() else {
        return park("no DeepBook deployment in token-info", &readiness).await;
    };
    let handles = DeepBookHandles {
        package: db.package()?,
        original_package: db.original_package()?,
        registry: db.registry()?,
    };
    let deep_coin_type = db.deep_coin_type.clone();
    let pool_creation_fee = db.pool_creation_fee_units().unwrap_or(500_000_000);

    let tokens: Vec<sim::SimToken> = snapshot
        .tokens()
        .iter()
        .map(|t| sim::SimToken {
            symbol: t.ticker.clone(),
            coin_type: t.coin_type.clone(),
            decimals: t.decimals,
            feed: t
                .pyth_feed_id
                .as_deref()
                .and_then(|f| protocol_types::PriceFeedId::from_hex(f).ok()),
        })
        .collect();

    let liquidity = FaucetMinter::new(snapshot.maybe_test_tokens(), cfg.gas_budget);

    // Live prices from oracle-service (the single Pyth gateway) over its WS
    // fanout. The banding pass skips (warn) until feeds arrive.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    let (price_cache, _ws_task) = oracle.subscribe();

    let staleness = pyth_client::Staleness {
        max_price_age: Duration::from_millis(cfg.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.max_publish_lag_ms),
        max_conf_bps: cfg.max_conf_bps,
    };

    // Config, secrets, the token-info snapshot and the DeepBook handles are
    // behind us. `sim::run` below is the steady-state loop, and its failure is
    // a park rather than an exit (see the module doc) — so nothing after this
    // point can fail startup (SO-324).
    readiness.ready();

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        pairs = cfg.spot_pairs.len(),
        interval_secs = cfg.spot_interval_secs,
        band_bps = cfg.spot_band_bps,
        "market-sim armed (spot bands)"
    );

    let params = sim::SimParams {
        cfg,
        secrets,
        network: Network::Testnet,
        handles,
        deep_coin_type,
        pool_creation_fee,
        liquidity,
        price_cache,
        staleness,
        tokens,
    };
    match sim::run(&params).await {
        Ok(()) => park("band loop returned (no usable pairs)", &readiness).await,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "[sim] band loop failed");
            park("band loop failed — see error above", &readiness).await
        }
    }
}

/// Log why the sim is inert and idle forever with /health green.
///
/// Parking flips readiness deliberately: per the module doc a failed gate is a
/// *healthy inert* state, not a startup failure. Leaving /health at 503 here
/// would turn every legitimately-disabled sim into a deploy rollback, which is
/// the opposite of the posture this service was given (SO-324).
///
/// The six call sites are two categories, and the flip only does work for one:
///
/// - **Gate cases** (disabled, non-testnet, no faucets, no DeepBook) park
///   *before* the flip in `main`, so this is the only thing that makes them
///   ready. That is the case this argument exists for.
/// - **Failure cases** (band loop returned / errored) park *after* it, so
///   `ready()` here is a no-op — readiness is already true and there is no
///   un-ready operation.
///
/// So a market-sim whose band loop dies does keep reporting ready. That is
/// unchanged from before SO-324 (/health was unconditionally green at all six
/// sites) and it is not something this flip decides. Making a failed band loop
/// visible to the gate means a *revocable* readiness plus a ruling on whether
/// a broken sim should roll back the fleet — deliberately out of scope here.
async fn park(reason: &str, readiness: &observability::ops::Readiness) -> Result<()> {
    warn!(reason, "[sim] parked — serving health/metrics only");
    readiness.ready();
    std::future::pending::<()>().await;
    Ok(())
}
