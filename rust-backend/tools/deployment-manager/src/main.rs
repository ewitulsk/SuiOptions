//! Deploys the options-protocol contracts tree (four Move packages) to a
//! Sui network and records every important on-chain address into a single
//! `deployments.json`.
//!
//! Pipeline per network, in dependency order (each publish stamps the
//! package's `Published.toml` so the next build links the fresh id):
//!   1. Publish `auction` (generic venue, no deps)
//!   2. Publish `core` (options_core) and parse object_changes for:
//!      package_id, AdminCap, ProtocolConfig, UpgradeCap
//!   3. Publish `rfq` (options_rfq: deps core + auction)
//!   4. Publish `vault` (options_vault: deps core + auction + pyth)
//!   5. Call `treasury::create_and_share(&AdminCap)` and capture the Treasury ID
//!   6. Merge into `deployments.json`, replacing only the targeted env's entry

use anyhow::{Context, Result};
use clap::Parser;
use sui_sdk::SuiClientBuilder;

use deployment_manager::deploy::{
    create_and_share_treasury, publish_cctp_package, publish_dep_package, publish_package,
    publish_test_tokens,
};
use deployment_manager::json_store::{
    CctpBridgeRecord, Deployments, NetworkDeployment, PackageInfo, PackageRecord,
    TestTokenRecord, TestTokensRecord, TokenSpec, TradingVaultObjectsRecord,
};
use deployment_manager::network::Network;
use deployment_manager::signer::Signer;
use deployment_manager::Cli;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let network = cli.network;
    let rpc_url = cli
        .rpc
        .clone()
        .unwrap_or_else(|| network.rpc_url().to_owned());
    let env_key = cli.env.to_ascii_lowercase();

    let mut store = Deployments::load_or_default(&output_path)?;

    // --deploy-cctp publishes ONLY the cctp_bridge package and records it on
    // the existing env entry — no protocol republish.
    if cli.deploy_cctp {
        let cctp_path = cli.cctp_contracts.canonicalize().with_context(|| {
            format!("resolving cctp-contracts path {}", cli.cctp_contracts.display())
        })?;
        let mut record = store
            .envs
            .get(&env_key)
            .cloned()
            .with_context(|| format!("env {env_key} not found in deployments.json — deploy the protocol first"))?;

        let signer = Signer::from_secrets(&secrets, network).context("loading signer")?;
        let client = SuiClientBuilder::default()
            .build(&rpc_url)
            .await
            .with_context(|| format!("building Sui client for {network}"))?;
        let outcome = publish_cctp_package(&client, &signer, &cctp_path, network, cli.gas_budget)
            .await
            .with_context(|| format!("publishing cctp_bridge to {network}"))?;
        tracing::info!(package = %outcome.package_id, "cctp_bridge published");

        record.package_info.cctp_bridge = Some(CctpBridgeRecord {
            package_id: outcome.package_id.to_string(),
            upgrade_cap_id: outcome.upgrade_cap_id.to_string(),
            publish_digest: outcome.digest,
            deployed_at: chrono::Utc::now().to_rfc3339(),
            network: network.as_str().to_owned(),
        });
        store.upsert(&env_key, record);
        store.save(&output_path)?;
        tracing::info!(path = %output_path.display(), env = %env_key, "cctpBridge recorded");
        return Ok(());
    }

    // --deploy-mm-collateral publishes ONLY the mm_collateral template and
    // writes mm-bot's state file — no deployments.json involvement (the ids
    // are one MM's private routing, not protocol infrastructure; they reach
    // the bot via the committed state file riding the deploy bundle).
    if cli.deploy_mm_collateral {
        let mm_path = cli.mm_collateral_contracts.canonicalize().with_context(|| {
            format!(
                "resolving mm-collateral path {}",
                cli.mm_collateral_contracts.display()
            )
        })?;
        let signer = Signer::from_secrets(&secrets, network).context("loading signer")?;
        let client = SuiClientBuilder::default()
            .build(&rpc_url)
            .await
            .with_context(|| format!("building Sui client for {network}"))?;
        let dep = move_publish::collateral::deploy(
            &client,
            &signer.keypair,
            signer.address,
            &mm_path,
            network.as_str(),
            cli.gas_budget,
        )
        .await
        .with_context(|| format!("publishing mm_collateral to {network}"))?;
        let out = cli.collateral_out.clone().unwrap_or_else(|| {
            PathBuf::from(format!(
                "services/mm-bot/config/collateral.{}.toml",
                network.as_str()
            ))
        });
        move_publish::collateral::store(&out, &dep)?;
        tracing::info!(
            path = %out.display(),
            package = %dep.package_id,
            account = %dep.account_id,
            env = %env_key,
            "mm_collateral recorded"
        );
        return Ok(());
    }

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
    let previous_cctp = store
        .envs
        .get(&env_key)
        .and_then(|d| d.package_info.cctp_bridge.clone());
    let record = deploy_one(
        network,
        &rpc_url,
        &secrets,
        &contracts_path,
        test_tokens_path.as_deref(),
        previous_tokens,
        previous_token_info,
        previous_deepbook,
        previous_cctp,
        deployment_manager::trading_vault_init::registrar_pubkey_for_env(&env_key),
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
    contracts_root: &std::path::Path,
    test_tokens_path: Option<&std::path::Path>,
    previous_tokens: Option<TestTokensRecord>,
    previous_token_info: BTreeMap<String, TokenSpec>,
    previous_deepbook: Option<serde_json::Value>,
    previous_cctp: Option<CctpBridgeRecord>,
    // Ed25519 pubkey of this env's attestation registrar (SO-308), seeded
    // into the VaultProtocolConfig at activation. `None` leaves the attested
    // registration path disabled.
    registrar_pubkey: Option<&str>,
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

    let env = network.as_str();
    let record = |o: &deployment_manager::deploy::DepPublishOutcome| PackageRecord {
        package_id: o.package_id.to_string(),
        upgrade_cap_id: o.upgrade_cap_id.to_string(),
        publish_digest: o.digest.clone(),
        deployed_at: chrono::Utc::now().to_rfc3339(),
    };

    // Publish the tree in dependency order; each publish stamps its
    // Published.toml so the next build resolves the fresh id.
    let auction_out =
        publish_dep_package(&client, &signer, &contracts_root.join("auction"), "auction", env, gas_budget)
            .await
            .with_context(|| format!("publishing auction to {network}"))?;
    tracing::info!(package = %auction_out.package_id, "auction published");

    let publish = publish_package(&client, &signer, &contracts_root.join("core"), env, gas_budget)
        .await
        .with_context(|| format!("publishing options_core to {network}"))?;
    tracing::info!(
        package = %publish.package_id,
        admin_cap = %publish.admin_cap_id,
        protocol_config = %publish.protocol_config_id,
        digest = %publish.digest,
        "options_core published"
    );

    let rfq_out =
        publish_dep_package(&client, &signer, &contracts_root.join("rfq"), "options_rfq", env, gas_budget)
            .await
            .with_context(|| format!("publishing options_rfq to {network}"))?;
    tracing::info!(package = %rfq_out.package_id, "options_rfq published");

    let vault_out =
        publish_dep_package(&client, &signer, &contracts_root.join("vault"), "options_vault", env, gas_budget)
            .await
            .with_context(|| format!("publishing options_vault to {network}"))?;
    tracing::info!(package = %vault_out.package_id, "options_vault published");

    let trading_vault_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("trading-vault"),
        "trading_vault",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing trading_vault to {network}"))?;
    tracing::info!(package = %trading_vault_out.package_id, "trading_vault published");

    let oracle_pyth_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("oracle-pyth"),
        "oracle_pyth",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing oracle_pyth to {network}"))?;
    tracing::info!(package = %oracle_pyth_out.package_id, "oracle_pyth published");

    let deepbook_adapter_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("deepbook-adapter"),
        "deepbook_adapter",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing deepbook_adapter to {network}"))?;
    tracing::info!(package = %deepbook_adapter_out.package_id, "deepbook_adapter published");

    let options_adapter_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("options-adapter"),
        "options_adapter",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing options_adapter to {network}"))?;
    tracing::info!(package = %options_adapter_out.package_id, "options_adapter published");

    let equity_oracle_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("equity-oracle"),
        "equity_oracle",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing equity_oracle to {network}"))?;
    tracing::info!(package = %equity_oracle_out.package_id, "equity_oracle published");

    let dbm_oracle_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("dbm-oracle"),
        "dbm_oracle",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing dbm_oracle to {network}"))?;
    tracing::info!(package = %dbm_oracle_out.package_id, "dbm_oracle published");

    let (auction, rfq, vault) =
        (Some(record(&auction_out)), Some(record(&rfq_out)), Some(record(&vault_out)));
    let (trading_vault, oracle_pyth) =
        (Some(record(&trading_vault_out)), Some(record(&oracle_pyth_out)));
    let (deepbook_adapter, options_adapter) =
        (Some(record(&deepbook_adapter_out)), Some(record(&options_adapter_out)));

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

    // Activate the trading-vault family (SO-292): allowlist witnesses,
    // seed Pyth feeds from the catalog, and record the governance object
    // ids so services stop re-deriving them from publish digests. Pools
    // are allowlisted per roll by the option-scheduler, not here.
    let trading_vault_objects = {
        let objects = deployment_manager::trading_vault_init::resolve_objects(
            &client,
            &trading_vault_out.digest,
            &oracle_pyth_out.digest,
            &deepbook_adapter_out.digest,
            &options_adapter_out.digest,
            &equity_oracle_out.digest,
        )
        .await
        .context("resolving trading-vault governance objects")?;
        let activation_digest = deployment_manager::trading_vault_init::activate(
            &client,
            &signer,
            &objects,
            publish.admin_cap_id,
            trading_vault_out.package_id,
            oracle_pyth_out.package_id,
            deepbook_adapter_out.package_id,
            options_adapter_out.package_id,
            equity_oracle_out.package_id,
            dbm_oracle_out.package_id,
            &token_info,
            registrar_pubkey,
            gas_budget,
        )
        .await
        .context("activating trading-vault registries")?;
        tracing::info!(digest = %activation_digest, "trading-vault registries activated");
        Some(TradingVaultObjectsRecord {
            vault_protocol_config_id: objects.vault_protocol_config_id.to_string(),
            integration_registry_id: objects.integration_registry_id.to_string(),
            oracle_registry_id: objects.oracle_registry_id.to_string(),
            pyth_feed_registry_id: objects.pyth_feed_registry_id.to_string(),
            pool_allowlist_id: objects.pool_allowlist_id.to_string(),
            equity_book_id: Some(objects.equity_book_id.to_string()),
            vol_book_id: Some(objects.vol_book_id.to_string()),
            registrar_pubkey: registrar_pubkey.map(str::to_owned),
            activation_digest,
        })
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
            auction,
            rfq,
            vault,
            trading_vault,
            oracle_pyth,
            deepbook_adapter,
            options_adapter,
            equity_oracle: Some(record(&equity_oracle_out)),
            dbm_oracle: Some(record(&dbm_oracle_out)),
            trading_vault_objects,
            cctp_bridge: previous_cctp,
        },
        token_info,
    })
}
