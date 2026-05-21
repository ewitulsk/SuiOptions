//! Deploys the options-protocol Move package to one or all Sui networks and
//! records every important on-chain address into a single `deployments.json`.
//!
//! Pipeline per network:
//!   1. Build the Move package (`sui-move-build`)
//!   2. Publish via the SDK transaction builder (auto-selects gas)
//!   3. Parse object_changes for: package_id, AdminCap, ProtocolConfig, UpgradeCap
//!   4. Call `treasury::create_and_share(&AdminCap)` and capture the Treasury ID
//!   5. Merge into `deployments.json`, replacing only the targeted network's entry

use anyhow::{Context, Result};
use clap::Parser;
use sui_sdk::SuiClientBuilder;

use deployment_manager::deploy::{
    create_and_share_treasury, publish_package, publish_test_tokens,
};
use deployment_manager::json_store::{
    Deployments, NetworkDeployment, TestTokenRecord, TestTokensRecord,
};
use deployment_manager::network::Network;
use deployment_manager::signer::Signer;
use deployment_manager::Cli;
use std::collections::BTreeMap;

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
    let test_tokens_path = if cli.deploy_tokens {
        Some(
            cli.test_tokens.canonicalize().with_context(|| {
                format!("resolving test-tokens path {}", cli.test_tokens.display())
            })?,
        )
    } else {
        None
    };
    let output_path = cli.output;

    let secrets = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let targets: Vec<Network> = match cli.network {
        Some(n) => vec![n],
        None => Network::ALL.to_vec(),
    };

    let mut store = Deployments::load_or_default(&output_path)?;

    let mut failures: Vec<(Network, anyhow::Error)> = Vec::new();
    for net in &targets {
        // Carry forward the existing testTokens record so re-publishing the
        // options package without `--deploy-tokens` doesn't wipe it.
        let previous_tokens = store
            .networks
            .get(net.as_str())
            .and_then(|d| d.test_tokens.clone());
        match deploy_one(
            *net,
            &secrets,
            &contracts_path,
            test_tokens_path.as_deref(),
            previous_tokens,
            cli.gas_budget,
            cli.skip_init,
        )
        .await
        {
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
    secrets: &shared::Secrets,
    contracts_path: &std::path::Path,
    test_tokens_path: Option<&std::path::Path>,
    previous_tokens: Option<TestTokensRecord>,
    gas_budget: u64,
    skip_init: bool,
) -> Result<NetworkDeployment> {
    tracing::info!(network = %network, rpc = network.rpc_url(), "starting deployment");

    let signer = Signer::from_secrets(secrets, network).context("loading signer")?;
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

    let test_tokens = if let Some(path) = test_tokens_path {
        let outcome = publish_test_tokens(&client, &signer, path, gas_budget)
            .await
            .with_context(|| format!("publishing test-tokens to {network}"))?;
        tracing::info!(
            package = %outcome.package_id,
            count = outcome.tokens.len(),
            "test-tokens published"
        );
        let mut tokens = BTreeMap::new();
        for t in outcome.tokens {
            tokens.insert(
                t.symbol,
                TestTokenRecord {
                    coin_type: t.coin_type,
                    faucet_id: t.faucet_id.to_string(),
                    decimals: t.decimals,
                },
            );
        }
        Some(TestTokensRecord {
            package_id: outcome.package_id.to_string(),
            upgrade_cap_id: outcome.upgrade_cap_id.to_string(),
            publish_digest: outcome.digest,
            deployed_at: chrono::Utc::now().to_rfc3339(),
            tokens,
        })
    } else if let Some(prev) = previous_tokens {
        // No fresh tokens this run — preserve whatever the last deploy
        // recorded so re-publishing the options package alone doesn't
        // erase the faucets.
        tracing::info!(
            package = %prev.package_id,
            count = prev.tokens.len(),
            "preserving existing testTokens record (use --deploy-tokens to refresh)"
        );
        Some(prev)
    } else {
        None
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
        test_tokens,
    })
}
