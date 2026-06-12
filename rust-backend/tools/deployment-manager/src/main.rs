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
    create_and_share_treasury, publish_package, publish_session_package, publish_test_tokens,
};
use deployment_manager::json_store::{
    Deployments, NetworkDeployment, PackageInfo, SessionTokensRecord, TestTokenRecord,
    TestTokensRecord, TokenSpec,
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
    let session_path = if cli.deploy_session {
        Some(cli.session_contracts.canonicalize().with_context(|| {
            format!(
                "resolving session-contracts path {}",
                cli.session_contracts.display()
            )
        })?)
    } else {
        None
    };
    let output_path = cli.output;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let network = cli.network;
    let rpc_url = cli
        .rpc
        .clone()
        .unwrap_or_else(|| network.rpc_url().to_owned());
    let env_key = cli.env.to_ascii_lowercase();

    let mut store = Deployments::load_or_default(&output_path)?;

    // Carry forward the existing testTokens record + off-chain catalog so
    // re-publishing the options package without `--deploy-tokens` doesn't
    // wipe them. Keyed by the env slot we're (re)deploying.
    let previous_tokens = store
        .envs
        .get(&env_key)
        .and_then(|d| d.package_info.test_tokens.clone());
    let previous_token_info = store
        .envs
        .get(&env_key)
        .map(|d| d.token_info.clone())
        .unwrap_or_default();
    let previous_deepbook = store
        .envs
        .get(&env_key)
        .and_then(|d| d.package_info.deepbook.clone());
    let previous_session = store
        .envs
        .get(&env_key)
        .and_then(|d| d.package_info.session_tokens.clone());

    let record = deploy_one(
        network,
        &rpc_url,
        &secrets,
        &contracts_path,
        test_tokens_path.as_deref(),
        session_path.as_deref(),
        previous_tokens,
        previous_token_info,
        previous_deepbook,
        previous_session,
        cli.gas_budget,
        cli.skip_init,
    )
    .await
    .with_context(|| format!("deploying env {env_key} to {network}"))?;

    tracing::info!(env = %env_key, network = %network, package = %record.package_info.package_id, "deployment recorded");
    store.upsert(&env_key, record);
    store.save(&output_path)?;

    tracing::info!(path = %output_path.display(), env = %env_key, "deployment written");
    Ok(())
}

async fn deploy_one(
    network: Network,
    rpc_url: &str,
    secrets: &runtime_config::Secrets,
    contracts_path: &std::path::Path,
    test_tokens_path: Option<&std::path::Path>,
    session_path: Option<&std::path::Path>,
    previous_tokens: Option<TestTokensRecord>,
    previous_token_info: BTreeMap<String, TokenSpec>,
    previous_deepbook: Option<serde_json::Value>,
    previous_session: Option<SessionTokensRecord>,
    gas_budget: u64,
    skip_init: bool,
) -> Result<NetworkDeployment> {
    tracing::info!(network = %network, rpc = %rpc_url, "starting deployment");

    let signer = Signer::from_secrets(secrets, network).context("loading signer")?;
    tracing::info!(deployer = %signer.address, "signer loaded");

    let client = SuiClientBuilder::default()
        .build(rpc_url)
        .await
        .with_context(|| format!("building Sui client for {network}"))?;

    // The protocol package links against siws_session, so the session
    // package publishes FIRST (rewriting its Move.toml to the fresh id).
    let session_tokens = if let Some(path) = session_path {
        let outcome =
            publish_session_package(&client, &signer, path, network.as_str(), gas_budget)
                .await
                .with_context(|| format!("publishing siws_session to {network}"))?;
        tracing::info!(
            package = %outcome.package_id,
            registry = %outcome.registry_id,
            "siws_session published"
        );
        Some(SessionTokensRecord {
            package_id: outcome.package_id.to_string(),
            registry_id: outcome.registry_id.to_string(),
            upgrade_cap_id: outcome.upgrade_cap_id.to_string(),
            publish_digest: outcome.digest,
            deployed_at: chrono::Utc::now().to_rfc3339(),
        })
    } else if let Some(prev) = previous_session {
        tracing::info!(
            package = %prev.package_id,
            "preserving existing sessionTokens record (use --deploy-session to refresh)"
        );
        Some(prev)
    } else {
        tracing::warn!(
            "no sessionTokens record and --deploy-session not set; the protocol publish \
             links against whatever the session package's Move.toml currently points at"
        );
        None
    };

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

    // Build the off-chain catalog. If we deployed test tokens this run,
    // mirror those addresses into token_info (preserving any pythFeedId
    // that was already there). Otherwise carry the previous catalog
    // forward — re-publishing the protocol alone shouldn't drop it.
    let token_info = if let Some(tt) = test_tokens.as_ref() {
        let mut out = previous_token_info;
        for (sym, rec) in &tt.tokens {
            let pyth = out.get(sym).and_then(|s| s.pyth_feed_id.clone());
            out.insert(
                sym.clone(),
                TokenSpec {
                    coin_type: rec.coin_type.clone(),
                    decimals: rec.decimals,
                    pyth_feed_id: pyth,
                },
            );
        }
        out
    } else {
        previous_token_info
    };

    Ok(NetworkDeployment {
        package_info: PackageInfo {
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
            deepbook: previous_deepbook,
            session_tokens,
        },
        token_info,
    })
}
