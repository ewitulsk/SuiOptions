use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tracing::info;

use gas_station::{router, AppState, Cli, Config};
use sui_tx::tx::sponsor::BudgetPolicy;
use sui_tx::SuiClientWrapper;
use sui_types::base_types::ObjectID;
use token_info_client::TokenInfoClient;

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let sui = SuiClientWrapper::connect(&secrets, cfg.network)
        .await
        .context("connecting Sui client + sponsor signer")?;

    // Build the sponsor allowlist from token-info: the protocol package plus
    // the package every supported token's coin type lives in. Layer the
    // off-catalog `additional_allowed_packages` on top. token-info is a hard
    // dependency — if it's unreachable at boot we crash rather than sponsor a
    // stale or empty set.
    let snapshot = TokenInfoClient::new(&cfg.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching token-info from {}", cfg.token_info_url))?;

    let mut allowed: BTreeSet<ObjectID> = BTreeSet::new();
    allowed.insert(
        snapshot
            .package()
            .context("protocol package id from token-info")?,
    );
    for token in snapshot.tokens() {
        allowed.insert(
            coin_type_package(&token.coin_type)
                .with_context(|| format!("package of coin type {}", token.coin_type))?,
        );
    }
    for p in &cfg.additional_allowed_packages {
        allowed.insert(
            ObjectID::from_str(p.trim()).with_context(|| format!("invalid package id {p}"))?,
        );
    }
    let allowed_packages: Vec<ObjectID> = allowed.into_iter().collect();

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        sponsor = %sui.signer.address,
        allowed_packages = allowed_packages.len(),
        additional_allowed_packages = cfg.additional_allowed_packages.len(),
        threshold_mist = cfg.min_balance_threshold_mist,
        max_gas_budget_mist = cfg.max_gas_budget_mist,
        "gas-station starting"
    );

    let state = Arc::new(AppState {
        sui,
        allowed_packages,
        policy: BudgetPolicy {
            max_gas_budget: cfg.max_gas_budget_mist,
            min_gas_budget: cfg.min_gas_budget_mist,
            buffer_bps: cfg.gas_budget_buffer_bps,
        },
        min_balance_threshold_mist: cfg.min_balance_threshold_mist,
    });

    router::serve(cfg.bind_addr, state, &cfg.allowed_origins).await
}

/// The Move package an `0xpkg::module::Type` coin type lives in.
fn coin_type_package(coin_type: &str) -> Result<ObjectID> {
    let addr = coin_type
        .split_once("::")
        .map(|(addr, _)| addr.trim())
        .ok_or_else(|| anyhow!("coin type {coin_type} has no `::` package separator"))?;
    ObjectID::from_str(addr).map_err(Into::into)
}
