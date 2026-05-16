//! Deploys the options-protocol Move package to one or all Sui networks and
//! records every important on-chain address into a single `deployments.json`.
//!
//! Pipeline per network:
//!   1. Build the Move package (`sui-move-build`)
//!   2. Publish via the SDK transaction builder (auto-selects gas)
//!   3. Parse object_changes for: package_id, AdminCap, ProtocolConfig, UpgradeCap
//!   4. Call `treasury::create_and_share(&AdminCap)` and capture the Treasury ID
//!   5. Merge into `deployments.json`, replacing only the targeted network's entry

mod deploy;
mod json_store;
mod network;
mod signer;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use sui_sdk::SuiClientBuilder;

use crate::deploy::{create_and_share_treasury, publish_package};
use crate::json_store::{Deployments, NetworkDeployment};
use crate::network::Network;
use crate::signer::Signer;

#[derive(Parser, Debug)]
#[command(
    name = "deploy",
    version,
    about = "Deploy options-protocol contracts to Sui networks and record addresses."
)]
struct Cli {
    /// Network to deploy to. Omit to deploy to all three.
    #[arg(short, long, value_enum)]
    network: Option<Network>,

    /// Path to the Move package containing the contracts.
    /// Default assumes the manager is run from `rust-backend/`.
    #[arg(short, long, default_value = "../contracts")]
    contracts: PathBuf,

    /// Path to the JSON file that tracks deployments per network.
    #[arg(short, long, default_value = "deployments.json")]
    output: PathBuf,

    /// Gas budget (MIST) per transaction.
    #[arg(long, default_value_t = 500_000_000)]
    gas_budget: u64,

    /// Skip the post-publish `treasury::create_and_share` call. Use when
    /// re-publishing for testing and you don't need a fresh Treasury.
    #[arg(long)]
    skip_init: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let contracts_path = cli
        .contracts
        .canonicalize()
        .with_context(|| format!("resolving contracts path {}", cli.contracts.display()))?;
    let output_path = cli.output;

    let targets: Vec<Network> = match cli.network {
        Some(n) => vec![n],
        None => Network::ALL.to_vec(),
    };

    let mut store = Deployments::load_or_default(&output_path)?;

    let mut failures: Vec<(Network, anyhow::Error)> = Vec::new();
    for net in &targets {
        match deploy_one(*net, &contracts_path, cli.gas_budget, cli.skip_init).await {
            Ok(record) => {
                tracing::info!(network = %net, package = %record.package_id, "deployment recorded");
                store.upsert(*net, record);
                // Persist after each network so a later failure doesn't lose
                // an earlier success.
                store.save(&output_path)?;
            }
            Err(e) => {
                tracing::error!(network = %net, error = %e, "deployment failed");
                failures.push((*net, e));
            }
        }
    }

    if !failures.is_empty() {
        eprintln!("\n{} network(s) failed:", failures.len());
        for (net, e) in &failures {
            eprintln!("  {net}: {e:#}");
        }
        std::process::exit(1);
    }

    tracing::info!(path = %output_path.display(), "all deployments written");
    Ok(())
}

async fn deploy_one(
    network: Network,
    contracts_path: &std::path::Path,
    gas_budget: u64,
    skip_init: bool,
) -> Result<NetworkDeployment> {
    tracing::info!(network = %network, rpc = network.rpc_url(), "starting deployment");

    let signer = Signer::load(network).context("loading signer")?;
    tracing::info!(deployer = %signer.address, "signer loaded");

    let client = SuiClientBuilder::default()
        .build(network.rpc_url())
        .await
        .with_context(|| format!("building Sui client for {network}"))?;

    let publish = publish_package(&client, &signer, contracts_path, gas_budget)
        .await
        .with_context(|| format!("publishing to {network}"))?;
    tracing::info!(
        package = %publish.package_id,
        admin_cap = %publish.admin_cap_id,
        protocol_config = %publish.protocol_config_id,
        digest = %publish.digest,
        "package published"
    );

    let (treasury_id, init_digest) = if skip_init {
        (None, None)
    } else {
        let init = create_and_share_treasury(
            &client,
            &signer,
            publish.package_id,
            publish.admin_cap_id,
            gas_budget,
        )
        .await
        .with_context(|| format!("initializing treasury on {network}"))?;
        tracing::info!(treasury = %init.treasury_id, "treasury created");
        (Some(init.treasury_id.to_string()), Some(init.digest))
    };

    Ok(NetworkDeployment {
        package_id: publish.package_id.to_string(),
        admin_cap_id: publish.admin_cap_id.to_string(),
        protocol_config_id: publish.protocol_config_id.to_string(),
        upgrade_cap_id: publish.upgrade_cap_id.to_string(),
        treasury_id,
        publish_digest: publish.digest,
        init_digest,
        deployer: signer.address.to_string(),
        deployed_at: chrono::Utc::now().to_rfc3339(),
        network: network.as_str().to_owned(),
    })
}
