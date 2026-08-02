use anyhow::{anyhow, Context, Result};
use move_publish::{finish_pubfile, stash_pubfile};
use std::path::Path;
use std::time::Duration;
use sui_move_build::BuildConfig;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::Identifier;
use sui_tx::chain::{created_objects, published_package, ChainClient, ExecutedTransaction};
use sui_types::base_types::ObjectID;
use sui_types::SUI_FRAMEWORK_ADDRESS;

pub use move_publish::DepPublishOutcome;

use crate::signer::Signer;

/// Parsed object IDs from the publish transaction.
pub struct PublishOutcome {
    pub package_id: ObjectID,
    pub admin_cap_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
}

/// Build the core Move package on disk, publish it via the SDK transaction
/// builder (auto-selects gas), and pull the IDs we care about out of the
/// response. Stamps the package's `Published.toml` with the fresh id on
/// success so downstream packages (options_rfq, options_vault) compile
/// against it.
pub async fn publish_package(
    client: &ChainClient,
    signer: &Signer,
    contracts_path: &Path,
    env_name: &str,
    gas_budget: u64,
) -> Result<PublishOutcome> {
    let stash = stash_pubfile(contracts_path)?;
    let result = publish_package_inner(client, signer, contracts_path, gas_budget).await;
    finish_pubfile(
        client,
        contracts_path,
        stash,
        env_name,
        result.as_ref().ok().map(|o| o.package_id),
    )
    .await?;
    result
}

async fn publish_package_inner(
    client: &ChainClient,
    signer: &Signer,
    contracts_path: &Path,
    gas_budget: u64,
) -> Result<PublishOutcome> {
    tracing::info!(path = %contracts_path.display(), "compiling Move package");
    let compiled = BuildConfig::new_for_testing()
        .build(contracts_path)
        .with_context(|| {
            format!("compiling Move package at {}", contracts_path.display())
        })?;

    let modules = compiled.get_package_bytes(/* with_unpublished_deps */ false);
    let deps = compiled.get_dependency_storage_package_ids();
    tracing::info!(modules = modules.len(), deps = deps.len(), "compiled");

    tracing::info!("submitting publish tx");
    let resp = submit_publish(client, signer, modules, deps, gas_budget, "publish").await?;
    extract_publish_outcome(&resp)
}

/// Outcome of publishing the cctp_bridge package.
pub struct CctpOutcome {
    pub package_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
}

/// Publish the cctp-contracts package (no init step; the package has no
/// one-time witness or shared state). Builds against the environment that
/// matches `network` so the resolver links Circle's published testnet or
/// mainnet packages (cctp-contracts/Move.toml [dep-replacements]).
pub async fn publish_cctp_package(
    client: &ChainClient,
    signer: &Signer,
    cctp_path: &Path,
    network: crate::network::Network,
    gas_budget: u64,
) -> Result<CctpOutcome> {
    tracing::info!(path = %cctp_path.display(), %network, "compiling cctp_bridge package");
    let mut build_config = BuildConfig::new_for_testing();
    build_config.environment = match network {
        crate::network::Network::Mainnet => sui_package_alt::mainnet_environment(),
        _ => sui_package_alt::testnet_environment(),
    };
    let compiled = build_config
        .build(cctp_path)
        .with_context(|| format!("compiling Move package at {}", cctp_path.display()))?;

    let modules = compiled.get_package_bytes(/* with_unpublished_deps */ false);
    let deps = compiled.get_dependency_storage_package_ids();
    tracing::info!(modules = modules.len(), deps = deps.len(), "compiled cctp_bridge");

    let resp = submit_publish(client, signer, modules, deps, gas_budget, "cctp publish").await?;

    let upgrade_cap_id = created_objects(&resp).into_iter().find_map(|c| {
        let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
        (tag.address == SUI_FRAMEWORK_ADDRESS
            && tag.module.as_str() == "package"
            && tag.name.as_str() == "UpgradeCap")
            .then_some(c.object_id)
    });
    Ok(CctpOutcome {
        package_id: published_package(&resp)
            .ok_or_else(|| anyhow!("cctp publish created no package"))?,
        upgrade_cap_id: upgrade_cap_id
            .ok_or_else(|| anyhow!("cctp publish created no UpgradeCap"))?,
        digest: sui_tx::tx::tx_digest(&resp).to_string(),
    })
}

/// Compile-free half of a publish: build the Publish PTB, pay gas, submit,
/// and assert success. Shared by the protocol and cctp publish paths.
async fn submit_publish(
    client: &ChainClient,
    signer: &Signer,
    modules: Vec<Vec<u8>>,
    deps: Vec<ObjectID>,
    gas_budget: u64,
    label: &str,
) -> Result<ExecutedTransaction> {
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap = pt.publish_upgradeable(modules, deps);
    pt.transfer_arg(signer.address, cap);
    sui_tx::tx::submit_ptb(client, signer, pt, gas_budget, label).await
}

fn extract_publish_outcome(resp: &ExecutedTransaction) -> Result<PublishOutcome> {
    let digest = sui_tx::tx::tx_digest(resp).to_string();

    let mut admin_cap_id: Option<ObjectID> = None;
    let mut protocol_config_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;

    for change in created_objects(resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&change.object_type) else {
            continue;
        };
        // 0x2::package::UpgradeCap is created for every publish.
        if tag.address == SUI_FRAMEWORK_ADDRESS
            && tag.module.as_str() == "package"
            && tag.name.as_str() == "UpgradeCap"
        {
            upgrade_cap_id = Some(change.object_id);
            continue;
        }
        // Match by module + struct name; the address is the freshly
        // published package, which we may not have captured yet.
        match (tag.module.as_str(), tag.name.as_str()) {
            ("admin", "AdminCap") => admin_cap_id = Some(change.object_id),
            ("admin", "ProtocolConfig") => protocol_config_id = Some(change.object_id),
            _ => {}
        }
    }

    Ok(PublishOutcome {
        package_id: published_package(resp)
            .ok_or_else(|| anyhow!("no published package in publish effects"))?,
        admin_cap_id: admin_cap_id
            .ok_or_else(|| anyhow!("AdminCap not found in object_changes"))?,
        protocol_config_id: protocol_config_id
            .ok_or_else(|| anyhow!("ProtocolConfig not found in object_changes"))?,
        upgrade_cap_id: upgrade_cap_id
            .ok_or_else(|| anyhow!("UpgradeCap not found in object_changes"))?,
        digest,
    })
}

pub struct InitOutcome {
    pub treasury_id: ObjectID,
    pub digest: String,
}

/// Call `treasury::create_and_share(&AdminCap)`. Idempotent on the read side
/// (it always creates a new Treasury), so callers must avoid double-running
/// this against the same deployment if they want a single canonical treasury.
pub async fn create_and_share_treasury(
    client: &ChainClient,
    signer: &Signer,
    package_id: ObjectID,
    admin_cap_id: ObjectID,
    gas_budget: u64,
) -> Result<InitOutcome> {
    // Give the fullnode a beat to index the freshly created AdminCap before
    // the high-level builder tries to fetch its current version/digest.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut pt = ProgrammableTransactionBuilder::new();
    let admin_cap_arg = pt.obj(client.owned_object_arg(admin_cap_id).await?)?;
    pt.programmable_move_call(
        package_id,
        Identifier::new("treasury")?,
        Identifier::new("create_and_share")?,
        vec![],
        vec![admin_cap_arg],
    );

    tracing::info!("submitting treasury init tx");
    let resp = sui_tx::tx::submit_ptb(
        client,
        signer,
        pt,
        gas_budget,
        "treasury::create_and_share",
    )
    .await?;

    let digest = sui_tx::tx::tx_digest(&resp).to_string();
    let treasury_id = created_objects(&resp)
        .into_iter()
        .find_map(|c| {
            let tag = sui_types::parse_sui_struct_tag(&c.object_type).ok()?;
            (tag.module.as_str() == "treasury" && tag.name.as_str() == "Treasury")
                .then_some(c.object_id)
        })
        .ok_or_else(|| anyhow!("Treasury object not found in init response"))?;

    Ok(InitOutcome { treasury_id, digest })
}

/// Publish one dependency package of the contracts tree (auction /
/// options_rfq / options_vault) and stamp its `Published.toml` so
/// downstream packages compile against the fresh id. Thin wrapper over the
/// shared [`move_publish`] crate (also used by mm-bot's deploy-collateral).
pub async fn publish_dep_package(
    client: &ChainClient,
    signer: &Signer,
    path: &Path,
    label: &str,
    env_name: &str,
    gas_budget: u64,
) -> Result<DepPublishOutcome> {
    move_publish::publish_dep_package(
        client,
        &signer.keypair,
        signer.address,
        path,
        label,
        env_name,
        gas_budget,
    )
    .await
}

/// Symbol → (module name, decimals). Hardcoded because Move modules name
/// their OTW after the module in uppercase, so module="tusdc" implies
/// type="TUSDC". Decimals match the real-world tokens they shadow.
const TEST_TOKEN_TABLE: &[(&str, &str, u8)] = &[
    ("TUSDC", "tusdc", 6),
    ("TBTC", "tbtc", 8),
    ("TWAL", "twal", 9),
    ("TSUI", "tsui", 9),
];

pub struct TestTokenInfo {
    pub symbol: String,
    pub coin_type: String,
    pub faucet_id: ObjectID,
    pub decimals: u8,
}

pub struct TestTokensOutcome {
    pub package_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
    pub tokens: Vec<TestTokenInfo>,
}

/// Compile + publish the test-tokens package. Each module's `init` creates a
/// shared Faucet<T>; we parse them out by module name and pair them with the
/// fixed `TEST_TOKEN_TABLE`. Re-publishing produces a fresh package each
/// time — callers are responsible for overwriting the JSON entry.
pub async fn publish_test_tokens(
    client: &ChainClient,
    signer: &Signer,
    tokens_path: &Path,
    gas_budget: u64,
) -> Result<TestTokensOutcome> {
    tracing::info!(path = %tokens_path.display(), "compiling test-tokens package");
    let compiled = BuildConfig::new_for_testing()
        .build(tokens_path)
        .with_context(|| {
            format!("compiling test-tokens package at {}", tokens_path.display())
        })?;
    let modules = compiled.get_package_bytes(false);
    let deps = compiled.get_dependency_storage_package_ids();

    tracing::info!("submitting test-tokens publish tx");
    let resp =
        submit_publish(client, signer, modules, deps, gas_budget, "test-tokens publish").await?;

    let digest = sui_tx::tx::tx_digest(&resp).to_string();
    let package_id = published_package(&resp);

    let mut upgrade_cap_id: Option<ObjectID> = None;
    // Map module name (e.g. "tusdc") -> Faucet object id.
    let mut faucets: std::collections::HashMap<String, ObjectID> =
        std::collections::HashMap::new();

    for change in created_objects(&resp) {
        let Ok(tag) = sui_types::parse_sui_struct_tag(&change.object_type) else {
            continue;
        };
        if tag.address == SUI_FRAMEWORK_ADDRESS
            && tag.module.as_str() == "package"
            && tag.name.as_str() == "UpgradeCap"
        {
            upgrade_cap_id = Some(change.object_id);
            continue;
        }
        if tag.name.as_str() == "Faucet" {
            faucets.insert(tag.module.as_str().to_owned(), change.object_id);
        }
    }

    let package_id = package_id
        .ok_or_else(|| anyhow!("test-tokens publish: no Published change"))?;
    let upgrade_cap_id = upgrade_cap_id
        .ok_or_else(|| anyhow!("test-tokens publish: no UpgradeCap created"))?;

    let pkg_hex = package_id.to_hex_uncompressed();
    let mut tokens = Vec::with_capacity(TEST_TOKEN_TABLE.len());
    for (symbol, module, decimals) in TEST_TOKEN_TABLE {
        let faucet_id = faucets
            .remove(*module)
            .ok_or_else(|| anyhow!("test-tokens publish: no Faucet for module `{module}`"))?;
        tokens.push(TestTokenInfo {
            symbol: (*symbol).to_owned(),
            coin_type: format!("{}::{}::{}", pkg_hex, module, symbol),
            faucet_id,
            decimals: *decimals,
        });
    }

    Ok(TestTokensOutcome {
        package_id,
        upgrade_cap_id,
        digest,
        tokens,
    })
}

