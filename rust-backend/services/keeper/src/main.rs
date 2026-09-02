//! keeper — permissionless liveness layer for the curated trading vaults.
//!
//! Boot:
//!   1. Parse Cli, load config + secrets, fetch the token-info snapshot
//!      (protocol ids, governance objects).
//!   2. Connect SuiClient. Any funded wallet works — the keeper holds no
//!      capability objects; the contracts validate every crank. (The
//!      equity-poster path additionally needs an allowlisted wallet.)
//!
//! Tick loop (every `tick_secs`, default 15): run the trading-vault pass
//! ([`keeper::trading_vault::tick`]) — settle finished RFQ auctions, redeem
//! expired positions, sweep DeepBook and `vault_mm` transfer-ins, post
//! external-account equity, and fulfill the withdrawal queue with a composed
//! attestation-bearing appraisal.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::info;

use indexer_graphql::IndexerClient;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::pyth_update::PythHandles;
use sui_types::base_types::ObjectID;
use token_info_client::TokenInfoClient;

use keeper::config::KeeperConfig;
use keeper::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("keeper");

    let cli = Cli::parse();
    let cfg = KeeperConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    let readiness = observability::ops::Readiness::new();
    observability::ops::spawn(cfg.health_addr, &readiness);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    let protocol_config_id = snapshot.protocol_config()?;
    let treasury_id = snapshot.treasury()?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    info!(signer = %wrap.signer.address, "keeper wallet connected (gas only)");

    let pyth_handles = PythHandles {
        pyth_package: parse_id(&cfg.pyth.pyth_package_id, "pyth_package_id")?,
        wormhole_package: parse_id(&cfg.pyth.wormhole_package_id, "wormhole_package_id")?,
        pyth_state_id: parse_id(&cfg.pyth.pyth_state_id, "pyth_state_id")?,
        wormhole_state_id: parse_id(&cfg.pyth.wormhole_state_id, "wormhole_state_id")?,
        update_fee_mist: cfg.pyth.update_fee_mist,
        price_info_table_id: cfg
            .pyth
            .price_info_table_id
            .as_deref()
            .map(|s| parse_id(s, "price_info_table_id"))
            .transpose()?,
    };

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .default_headers(pyth_client::auth_headers(secrets.pyth_api_key()))
        .build()
        .context("building reqwest client")?;
    // Spot + realized vol come from oracle-service (the single Pyth gateway).
    // The keeper's own `http` stays for the on-chain VAA path, which a price
    // cache can't serve. Hard cutover: crash if the oracle never comes up.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    oracle
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("oracle-service at {} unreachable", cli.oracle_url))?;
    // Trading-vault pass (SO-287/290). Governance ids prefer token-info's
    // recorded block, falling back to publish-effects discovery. A build
    // failure is fatal (partial token-info snapshot from a same-wave deploy
    // boot race) — crash and let the supervisor retry against a warmed
    // token-info.
    let trading_vault_ctx = keeper::trading_vault::build_ctx(
        &wrap.client,
        &snapshot,
        treasury_id,
        protocol_config_id,
        cli.gas_budget,
        cfg.pyth.hermes_url.clone(),
        pyth_handles.clone(),
        &cfg.external,
        oracle.clone(),
        cfg.vault_defaults.vol_window_days,
        cfg.mark_refresh_interval_ms,
    )
    .await
    .context("building the trading-vault ctx (token-info snapshot incomplete or chain unreachable)")?;
    let indexer = IndexerClient::new(cfg.indexer_graphql_url.clone());
    info!(
        indexer = %cfg.indexer_graphql_url,
        trading_vault = trading_vault_ctx.is_some(),
        "keeper configured"
    );

    // Everything fallible is behind us: config, secrets, the token-info
    // snapshot, the Sui client, the oracle handshake and the trading-vault
    // ctx. Nothing below this line can fail startup, so this is where
    // "started" becomes "ready" (SO-324).
    readiness.ready();

    let tick = Duration::from_secs(cfg.tick_secs.max(1));
    info!(tick_secs = cfg.tick_secs, dry_run = cli.dry_run, "tick loop starting");
    loop {
        metrics::counter!("keeper_ticks_total").increment(1);
        let tick_started = std::time::Instant::now();
        if let Some(tvc) = &trading_vault_ctx {
            keeper::trading_vault::tick(&wrap, &http, &indexer, tvc).await;
        }
        metrics::histogram!("keeper_tick_duration_seconds").record(tick_started.elapsed().as_secs_f64());
        sleep(tick).await;
    }
}

fn parse_id(s: &str, what: &str) -> Result<ObjectID> {
    ObjectID::from_str(s).with_context(|| format!("parsing {what} {s:?}"))
}
