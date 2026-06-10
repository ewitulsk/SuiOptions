use anyhow::{anyhow, bail, Context, Result};
use shared_crypto::intent::Intent;
use std::path::Path;
use std::time::Duration;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_move_build::BuildConfig;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::transaction::Transaction;
use sui_types::SUI_FRAMEWORK_ADDRESS;

use crate::signer::Signer;

/// Parsed object IDs from the publish transaction.
pub struct PublishOutcome {
    pub package_id: ObjectID,
    pub admin_cap_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub upgrade_cap_id: ObjectID,
    pub digest: String,
}

/// Build the Move package on disk, publish it via the SDK transaction builder
/// (auto-selects gas), and pull the IDs we care about out of the response.
pub async fn publish_package(
    client: &SuiClient,
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

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building publish tx")?;

    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);

    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting publish tx")?;

    assert_success(&resp)?;
    extract_publish_outcome(&resp)
}

fn extract_publish_outcome(resp: &SuiTransactionBlockResponse) -> Result<PublishOutcome> {
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut admin_cap_id: Option<ObjectID> = None;
    let mut protocol_config_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;

    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => {
                package_id = Some(*pid);
            }
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } => {
                // 0x2::package::UpgradeCap is created for every publish.
                if object_type.address == SUI_FRAMEWORK_ADDRESS
                    && object_type.module.as_str() == "package"
                    && object_type.name.as_str() == "UpgradeCap"
                {
                    upgrade_cap_id = Some(*object_id);
                    continue;
                }
                // Match by module + struct name; the address is the freshly
                // published package, which we may not have captured yet.
                match (object_type.module.as_str(), object_type.name.as_str()) {
                    ("admin", "AdminCap") => admin_cap_id = Some(*object_id),
                    ("admin", "ProtocolConfig") => protocol_config_id = Some(*object_id),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(PublishOutcome {
        package_id: package_id.ok_or_else(|| anyhow!("no Published object_change in response"))?,
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
    client: &SuiClient,
    signer: &Signer,
    package_id: ObjectID,
    admin_cap_id: ObjectID,
    gas_budget: u64,
) -> Result<InitOutcome> {
    // Give the fullnode a beat to index the freshly created AdminCap before
    // the high-level builder tries to fetch its current version/digest.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let admin_cap_arg = sui_json::SuiJsonValue::from_object_id(admin_cap_id);

    let tx_data = client
        .transaction_builder()
        .move_call(
            signer.address,
            package_id,
            "treasury",
            "create_and_share",
            vec![],
            vec![admin_cap_arg],
            None,
            gas_budget,
            None,
        )
        .await
        .context("building treasury::create_and_share tx")?;

    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);

    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting treasury init tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting treasury init tx")?;

    assert_success(&resp)?;
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("init response missing object_changes"))?;

    let treasury_id = changes
        .iter()
        .find_map(|c| match c {
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } if object_type.module.as_str() == "treasury"
                && object_type.name.as_str() == "Treasury" =>
            {
                Some(*object_id)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow!("Treasury object not found in init response"))?;

    Ok(InitOutcome { treasury_id, digest })
}

pub struct OrgOutcome {
    pub org_id: ObjectID,
    pub org_cap_id: ObjectID,
    pub digest: String,
}

/// Call `org::create_org(name, fee_bps)` and transfer the returned `OrgCap`
/// to the deployer. This creates the platform's own org ("SuiOptions") so the
/// option-scheduler can roll buckets under it. `create_org` is permissionless;
/// the deploy step just bootstraps the first one and records its ids.
pub async fn create_platform_org(
    client: &SuiClient,
    signer: &Signer,
    package_id: ObjectID,
    name: &str,
    fee_bps: u64,
    gas_budget: u64,
) -> Result<OrgOutcome> {
    use move_core_types::identifier::Identifier;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::TransactionData;

    // Give the fullnode a beat to index the freshly published package.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut pt = ProgrammableTransactionBuilder::new();
    let arg_name = pt.pure(name.to_string())?;
    let arg_fee = pt.pure(fee_bps)?;
    let cap = pt.programmable_move_call(
        package_id,
        Identifier::new("org").unwrap(),
        Identifier::new("create_org").unwrap(),
        vec![],
        vec![arg_name, arg_fee],
    );
    pt.transfer_arg(signer.address, cap);
    let programmable = pt.finish();

    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no SUI coins to pay gas for {}", signer.address))?;
    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;
    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin.object_ref()],
        programmable,
        gas_budget,
        gas_price,
    );
    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!(name, fee_bps, "submitting org::create_org tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting create_org tx")?;
    assert_success(&resp)?;

    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("create_org response missing object_changes"))?;

    let mut org_id: Option<ObjectID> = None;
    let mut org_cap_id: Option<ObjectID> = None;
    for change in changes {
        if let ObjectChange::Created {
            object_id,
            object_type,
            ..
        } = change
        {
            match (object_type.module.as_str(), object_type.name.as_str()) {
                ("org", "Org") => org_id = Some(*object_id),
                ("org", "OrgCap") => org_cap_id = Some(*object_id),
                _ => {}
            }
        }
    }

    Ok(OrgOutcome {
        org_id: org_id.ok_or_else(|| anyhow!("Org object not found in create_org response"))?,
        org_cap_id: org_cap_id
            .ok_or_else(|| anyhow!("OrgCap object not found in create_org response"))?,
        digest,
    })
}

/// Symbol → (module name, decimals). Hardcoded because Move modules name
/// their OTW after the module in uppercase, so module="tusdc" implies
/// type="TUSDC". Decimals match the real-world tokens they shadow.
const TEST_TOKEN_TABLE: &[(&str, &str, u8)] = &[
    ("TUSDC", "tusdc", 6),
    ("TBTC", "tbtc", 8),
    ("TWAL", "twal", 9),
    ("TDEEP", "tdeep", 6),
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
    client: &SuiClient,
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

    let tx_data = client
        .transaction_builder()
        .publish(signer.address, modules, deps, None, gas_budget)
        .await
        .context("building test-tokens publish tx")?;
    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();

    tracing::info!("submitting test-tokens publish tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting test-tokens publish tx")?;
    assert_success(&resp)?;

    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("test-tokens publish response missing object_changes"))?;

    let mut package_id: Option<ObjectID> = None;
    let mut upgrade_cap_id: Option<ObjectID> = None;
    // Map module name (e.g. "tusdc") -> Faucet object id.
    let mut faucets: std::collections::HashMap<String, ObjectID> =
        std::collections::HashMap::new();

    for change in changes {
        match change {
            ObjectChange::Published { package_id: pid, .. } => {
                package_id = Some(*pid);
            }
            ObjectChange::Created {
                object_id,
                object_type,
                ..
            } => {
                if object_type.address == SUI_FRAMEWORK_ADDRESS
                    && object_type.module.as_str() == "package"
                    && object_type.name.as_str() == "UpgradeCap"
                {
                    upgrade_cap_id = Some(*object_id);
                    continue;
                }
                if object_type.name.as_str() == "Faucet" {
                    faucets.insert(object_type.module.as_str().to_owned(), *object_id);
                }
            }
            _ => {}
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

fn assert_success(resp: &SuiTransactionBlockResponse) -> Result<()> {
    let effects = resp
        .effects
        .as_ref()
        .ok_or_else(|| anyhow!("response missing effects"))?;
    if effects.status().is_err() {
        bail!("tx failed: {:?}", effects.status());
    }
    Ok(())
}
