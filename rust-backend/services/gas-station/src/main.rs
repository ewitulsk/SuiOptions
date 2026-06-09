use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;

use gas_station::{router, AppState, Cli, Config};
use sui_tx::tx::sponsor::BudgetPolicy;
use sui_tx::SuiClientWrapper;
use sui_types::base_types::ObjectID;

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

    let allowed_packages = cfg
        .allowed_packages
        .iter()
        .map(|p| ObjectID::from_str(p.trim()).with_context(|| format!("invalid package id {p}")))
        .collect::<Result<Vec<_>>>()?;

    info!(
        environment = %cfg.environment,
        network = %cfg.network,
        sponsor = %sui.signer.address,
        allowed_packages = allowed_packages.len(),
        threshold_mist = cfg.min_balance_threshold_mist,
        max_gas_budget_mist = cfg.max_gas_budget_mist,
        "gas-station starting"
    );
    if allowed_packages.is_empty() {
        tracing::warn!("allowed_packages is empty — the station will sponsor ANY package");
    }

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
