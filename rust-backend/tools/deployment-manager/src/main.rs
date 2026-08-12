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
//!   4. Publish the trading-vault family and its adapters/oracles
//!   5. Call `treasury::create_and_share(&AdminCap)` and capture the Treasury ID
//!   6. Merge into `deployments.json`, replacing only the targeted env's entry
//!
//! `vault` (options_vault) is deliberately absent: the covered-call vault
//! product is deprecated (SO-332) and is no longer published.

use anyhow::{Context, Result};
use clap::Parser;
use sui_tx::chain::ChainClient;

use deployment_manager::deploy::{
    create_and_share_treasury, publish_cctp_package, publish_dep_package, publish_package,
    publish_test_tokens,
};
use deployment_manager::json_store::{
    CctpBridgeRecord, Deployments, ExchangeRecord, NetworkDeployment, PackageInfo,
    PackageRecord, TestTokenRecord, TestTokensRecord, TokenSpec, TradingVaultObjectsRecord,
};
use deployment_manager::network::Network;
use deployment_manager::signer::load_signer;
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
    // Endpoint precedence: --grpc flag, then the operator's shared override
    // from the secrets file (the same `[sui] grpc_url` every service reads),
    // then the public default. The old JSON-RPC path silently fell back to a
    // deactivated public fullnode and failed at the first read.
    let grpc_url = cli
        .grpc
        .clone()
        .unwrap_or_else(|| secrets.resolve_grpc_url(network.grpc_url()));
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

        let signer = load_signer(&secrets, network).context("loading signer")?;
        let client = ChainClient::new(&grpc_url)
            .with_context(|| format!("building chain client for {network}"))?;
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

    // --deploy-exchange publishes ONLY the hybrid-exchange settlement
    // package (+ fresh markets) and records it on the existing env entry —
    // no protocol republish. The default pipeline also republishes the
    // exchange on every redeploy; this flag is for exchange-only iteration.
    if cli.deploy_exchange {
        let exchange_path = contracts_path.join("exchange");
        let mut record = store
            .envs
            .get(&env_key)
            .cloned()
            .with_context(|| format!("env {env_key} not found in deployments.json — deploy the protocol first"))?;

        let signer = load_signer(&secrets, network).context("loading signer")?;
        let client = ChainClient::new(&grpc_url)
            .with_context(|| format!("building chain client for {network}"))?;
        let outcome = move_publish::publish_dep_package(
            &client,
            &signer.keypair,
            signer.address,
            &exchange_path,
            "exchange",
            network.as_str(),
            cli.gas_budget,
        )
        .await
        .with_context(|| format!("publishing exchange to {network}"))?;
        // exchange::admin's init transfers an AdminCap to the deployer.
        let admin_cap_id = outcome
            .created_objects
            .iter()
            .find(|(module, name, _)| module == "admin" && name == "AdminCap")
            .map(|(_, _, id)| *id)
            .context("exchange publish created no admin::AdminCap")?;
        tracing::info!(
            package = %outcome.package_id,
            admin_cap = %admin_cap_id,
            "exchange published"
        );

        let mut exchange = ExchangeRecord {
            package_id: outcome.package_id.to_string(),
            upgrade_cap_id: outcome.upgrade_cap_id.to_string(),
            admin_cap_id: admin_cap_id.to_string(),
            publish_digest: outcome.digest,
            deployed_at: chrono::Utc::now().to_rfc3339(),
            network: network.as_str().to_owned(),
            markets: std::collections::BTreeMap::new(),
        };
        // A fresh package has no markets yet: list every token × TUSDC now
        // so the publish ceremony ends with a tradeable exchange.
        deployment_manager::exchange_markets::create_markets(
            &client,
            &signer,
            &mut exchange,
            &record.token_info,
            cli.gas_budget,
        )
        .await
        .context("creating exchange markets")?;
        record.package_info.exchange = Some(exchange);
        store.upsert(&env_key, record);
        store.save(&output_path)?;
        tracing::info!(path = %output_path.display(), env = %env_key, "exchange recorded");
        return Ok(());
    }

    // --deploy-mm-collateral publishes the mm_collateral template and writes
    // mm-bot's state file (the collateral ids are one MM's private routing —
    // they reach the bot via the committed state file riding the deploy
    // bundle, not deployments.json). It then creates the deployment's
    // QuoteSigner and records THAT id in deployments.json: the signer id is
    // protocol-facing (quote verification), and this is the only pass signed
    // by the MM-BOT key, which must own the signer.
    if cli.deploy_mm_collateral {
        let mm_path = cli.mm_collateral_contracts.canonicalize().with_context(|| {
            format!(
                "resolving mm-collateral path {}",
                cli.mm_collateral_contracts.display()
            )
        })?;
        let signer = load_signer(&secrets, network).context("loading signer")?;
        let client = ChainClient::new(&grpc_url)
            .with_context(|| format!("building chain client for {network}"))?;
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

        // The core republish that preceded this pass also invalidated the
        // previous deployment's QuoteSigner (its Move type is bound to the
        // old package id). This is the only ceremony step signing with the
        // MM-BOT key — and the signer's on-chain `owner` is the tx sender —
        // so create the fresh one here and record it in deployments.json
        // for token-info to serve. mm-bot verifies the id against chain
        // state before adopting it, exactly as it does a config-pinned id.
        let mut record = store.envs.get(&env_key).cloned().with_context(|| {
            format!("env {env_key} not found in deployments.json — deploy the protocol first")
        })?;
        let core_package = sui_types::base_types::ObjectID::from_hex_literal(
            &record.package_info.package_id,
        )
        .context("parsing core package_id from deployments.json")?;
        // Every env's mm-bot config uses the default ed25519 scheme; the
        // key's flag byte is validated against it in from_secret_str.
        let quote = sui_tx::quote_signer::QuoteSigner::from_secret_str(
            secrets.mm_quote_key().context(
                "mm-secrets file has no [mm_bot] quote_key — required to create the QuoteSigner",
            )?,
            protocol_types::SigningScheme::Ed25519,
        )
        .context("loading mm quote key")?;
        let created = sui_tx::tx::signer::create_and_share_signer(
            &client,
            &signer,
            core_package,
            quote.scheme(),
            &quote.public_bytes(),
            cli.gas_budget,
        )
        .await
        .context("creating on-chain QuoteSigner")?;
        record.package_info.quote_signer_id = Some(created.signer_id.to_string());
        store.upsert(&env_key, record);
        store.save(&output_path)?;
        tracing::info!(
            signer_id = %created.signer_id,
            digest = %created.digest,
            env = %env_key,
            "quoteSignerId recorded"
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
        &grpc_url,
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
    grpc_url: &str,
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
    let signer = load_signer(secrets, network).context("loading signer")?;
    let client = ChainClient::new(grpc_url)
        .with_context(|| format!("building chain client for {network}"))?;
    tracing::info!(
        network = %network,
        grpc_host = client.host(),
        deployer = %signer.address,
        "starting deployment"
    );

    let env = network.as_str();
    let record = |o: &deployment_manager::deploy::DepPublishOutcome| PackageRecord {
        package_id: o.package_id.to_string(),
        upgrade_cap_id: o.upgrade_cap_id.to_string(),
        publish_digest: o.digest.clone(),
        deployed_at: chrono::Utc::now().to_rfc3339(),
    };

    // Publish the tree in dependency order; each publish stamps its
    // Published.toml so the next build resolves the fresh id.
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

    // `vault` (options_vault) is NOT published: the covered-call vault product
    // is deprecated (SO-332). Neither are `auction` and `options_rfq`: the
    // on-chain RFQ/auction venue is retired (the desk writes through the
    // VaultMm quote path). All three live under contracts/.deprecated/ —
    // see their DEPRECATED.md files. Fresh deployment records carry no
    // `auction` / `rfq` / `vault` blocks; every consumer treats them as
    // optional. Do not reinstate a publish step without also re-enabling
    // the product's off-chain surface.
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

    // Both price adapters are published every deploy (SO-335). Publishing
    // only the "live" one would make a provider switch a redeploy, which
    // is exactly what the abstraction exists to avoid.
    let oracle_switchboard_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("oracle-switchboard"),
        "oracle_switchboard",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing oracle_switchboard to {network}"))?;
    tracing::info!(
        package = %oracle_switchboard_out.package_id,
        "oracle_switchboard published"
    );

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

    // All `None` — options_vault (SO-332) and the auction/options_rfq venue
    // are no longer published; see contracts/.deprecated/.
    let (auction, rfq, vault) = (None, None, None);
    let (trading_vault, oracle_pyth) =
        (Some(record(&trading_vault_out)), Some(record(&oracle_pyth_out)));
    let oracle_switchboard = Some(record(&oracle_switchboard_out));
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
            // Carry BOTH providers' feed keys forward: a redeploy must not
            // silently drop one and pin the env to a single provider.
            let pyth = out.get(sym).and_then(|s| s.pyth_feed_id.clone());
            let switchboard = out.get(sym).and_then(|s| s.switchboard_feed_id.clone());
            out.insert(
                sym.clone(),
                TokenSpec {
                    coin_type: rec.coin_type.clone(),
                    decimals: rec.decimals,
                    pyth_feed_id: pyth,
                    switchboard_feed_id: switchboard,
                },
            );
        }
        out
    } else {
        previous_token_info
    };

    // The exchange lives in the same redeploy cycle as the protocol: a
    // testnet redeploy invalidates open orders by definition, so the
    // settlement package republishes fresh every run and its markets are
    // recreated against the current token catalog. (The orderbook DB is
    // wiped by the redeploy workflow alongside indexer/scheduler, and its
    // whitelist sync disables the previous deployment's market rows.)
    //
    // Published BEFORE the trading-vault activation because the
    // exchange-adapter (SO-370) links against it and its witness joins
    // the activation PTB's integration allowlist.
    let exchange = {
        let out = publish_dep_package(
            &client,
            &signer,
            &contracts_root.join("exchange"),
            "exchange",
            env,
            gas_budget,
        )
        .await
        .with_context(|| format!("publishing exchange to {network}"))?;
        // exchange::admin's init transfers an AdminCap to the deployer.
        let admin_cap_id = out
            .created_objects
            .iter()
            .find(|(module, name, _)| module == "admin" && name == "AdminCap")
            .map(|(_, _, id)| *id)
            .context("exchange publish created no admin::AdminCap")?;
        tracing::info!(package = %out.package_id, admin_cap = %admin_cap_id, "exchange published");
        let mut ex = ExchangeRecord {
            package_id: out.package_id.to_string(),
            upgrade_cap_id: out.upgrade_cap_id.to_string(),
            admin_cap_id: admin_cap_id.to_string(),
            publish_digest: out.digest,
            deployed_at: chrono::Utc::now().to_rfc3339(),
            network: network.as_str().to_owned(),
            markets: std::collections::BTreeMap::new(),
        };
        deployment_manager::exchange_markets::create_markets(
            &client,
            &signer,
            &mut ex,
            &token_info,
            gas_budget,
        )
        .await
        .context("creating exchange markets")?;
        Some(ex)
    };

    // Vault-curator maker adapter for the hybrid exchange (SO-370);
    // republishes with the exchange it links against.
    let exchange_adapter_out = publish_dep_package(
        &client,
        &signer,
        &contracts_root.join("exchange-adapter"),
        "exchange_adapter",
        env,
        gas_budget,
    )
    .await
    .with_context(|| format!("publishing exchange_adapter to {network}"))?;
    tracing::info!(package = %exchange_adapter_out.package_id, "exchange_adapter published");

    // Activate the trading-vault family (SO-292): allowlist witnesses,
    // seed each provider's feeds from the catalog, and record the governance object
    // ids so services stop re-deriving them from publish digests. Pools
    // are allowlisted per roll by the option-scheduler, not here.
    let trading_vault_objects = {
        let objects = deployment_manager::trading_vault_init::resolve_objects(
            &trading_vault_out.created_objects,
            &oracle_pyth_out.created_objects,
            &oracle_switchboard_out.created_objects,
            &deepbook_adapter_out.created_objects,
            &options_adapter_out.created_objects,
            &equity_oracle_out.created_objects,
        )
        .context("resolving trading-vault governance objects")?;
        let activation_digest = deployment_manager::trading_vault_init::activate(
            &client,
            &signer,
            &objects,
            publish.admin_cap_id,
            trading_vault_out.package_id,
            oracle_pyth_out.package_id,
            oracle_switchboard_out.package_id,
            deepbook_adapter_out.package_id,
            options_adapter_out.package_id,
            exchange_adapter_out.package_id,
            equity_oracle_out.package_id,
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
            switchboard_feed_registry_id: Some(
                objects.switchboard_feed_registry_id.to_string(),
            ),
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
            oracle_switchboard,
            deepbook_adapter,
            options_adapter,
            exchange_adapter: Some(record(&exchange_adapter_out)),
            equity_oracle: Some(record(&equity_oracle_out)),
            trading_vault_objects,
            cctp_bridge: previous_cctp,
            exchange,
            // Deliberately not carried forward: a republish invalidates the
            // previous deployment's QuoteSigner (package-bound type). The
            // --deploy-mm-collateral pass fills it in.
            quote_signer_id: None,
        },
        token_info,
    })
}
