//! Post-publish activation for the trading-vault package family (SO-292):
//! resolve the shared governance objects each package's `init` created,
//! then run the activation PTB — allowlist the three integration
//! witnesses + the Pyth oracle witness, and seed the Pyth feed registry
//! from the token catalog. Without this a fresh deployment ships inert
//! (empty registries), which previously required manual PTBs after every
//! redeploy.
//!
//! Pool allowlisting is deliberately NOT done here: pools are created by
//! the option-scheduler per roll (and a redeploy wipes + re-rolls them),
//! so the scheduler allowlists each pool as it creates it.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, Transaction, TransactionData};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;

use move_publish::assert_success;

use crate::json_store::TokenSpec;
use crate::signer::Signer;

/// The shared objects the trading-vault family's inits create, recorded
/// into deployments.json so services stop re-deriving them from publish
/// digests.
#[derive(Debug, Clone)]
pub struct TradingVaultObjects {
    pub vault_protocol_config_id: ObjectID,
    pub integration_registry_id: ObjectID,
    pub oracle_registry_id: ObjectID,
    pub pyth_feed_registry_id: ObjectID,
    pub pool_allowlist_id: ObjectID,
}

/// Pull one publish tx's created objects and index them by
/// `module::name`.
async fn created_by_type(
    client: &SuiClient,
    digest: &str,
) -> Result<BTreeMap<String, ObjectID>> {
    let digest = digest
        .parse()
        .with_context(|| format!("parsing publish digest {digest}"))?;
    let resp = client
        .read_api()
        .get_transaction_with_options(
            digest,
            SuiTransactionBlockResponseOptions::new().with_object_changes(),
        )
        .await
        .context("fetching publish tx for object resolution")?;
    let mut out = BTreeMap::new();
    for change in resp.object_changes.unwrap_or_default() {
        if let ObjectChange::Created {
            object_id,
            object_type,
            ..
        } = change
        {
            out.insert(
                format!("{}::{}", object_type.module, object_type.name),
                object_id,
            );
        }
    }
    Ok(out)
}

pub async fn resolve_objects(
    client: &SuiClient,
    trading_vault_digest: &str,
    oracle_pyth_digest: &str,
    deepbook_adapter_digest: &str,
) -> Result<TradingVaultObjects> {
    let tv = created_by_type(client, trading_vault_digest).await?;
    let op = created_by_type(client, oracle_pyth_digest).await?;
    let dba = created_by_type(client, deepbook_adapter_digest).await?;
    let pick = |map: &BTreeMap<String, ObjectID>, key: &str| {
        map.get(key)
            .copied()
            .ok_or_else(|| anyhow!("{key} not found in publish effects"))
    };
    Ok(TradingVaultObjects {
        vault_protocol_config_id: pick(&tv, "registry::VaultProtocolConfig")?,
        integration_registry_id: pick(&tv, "registry::IntegrationRegistry")?,
        oracle_registry_id: pick(&tv, "registry::OracleRegistry")?,
        pyth_feed_registry_id: pick(&op, "oracle_pyth::PythFeedRegistry")?,
        pool_allowlist_id: pick(&dba, "deepbook_adapter::PoolAllowlist")?,
    })
}

async fn shared_mut_arg(client: &SuiClient, id: ObjectID) -> Result<ObjectArg> {
    let obj = client
        .read_api()
        .get_object_with_options(
            id,
            sui_json_rpc_types::SuiObjectDataOptions::new().with_owner(),
        )
        .await?
        .data
        .ok_or_else(|| anyhow!("shared object {id} missing"))?;
    let initial_shared_version = match obj.owner {
        Some(sui_types::object::Owner::Shared {
            initial_shared_version,
        }) => initial_shared_version,
        other => return Err(anyhow!("object {id} is not shared: {other:?}")),
    };
    Ok(ObjectArg::SharedObject {
        id,
        initial_shared_version,
        mutability: sui_types::transaction::SharedObjectMutability::Mutable,
    })
}

/// One PTB: witness allowlisting + feed seeding. Returns the digest.
#[allow(clippy::too_many_arguments)]
pub async fn activate(
    client: &SuiClient,
    signer: &Signer,
    objects: &TradingVaultObjects,
    admin_cap_id: ObjectID,
    trading_vault_pkg: ObjectID,
    oracle_pyth_pkg: ObjectID,
    deepbook_adapter_pkg: ObjectID,
    options_adapter_pkg: ObjectID,
    token_info: &BTreeMap<String, TokenSpec>,
    gas_budget: u64,
) -> Result<String> {
    // Let the fullnode index the freshly shared registries.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut pt = ProgrammableTransactionBuilder::new();

    let admin_ref = client
        .read_api()
        .get_object_with_options(
            admin_cap_id,
            sui_json_rpc_types::SuiObjectDataOptions::new(),
        )
        .await
        .context("fetching AdminCap")?
        .data
        .ok_or_else(|| anyhow!("AdminCap object missing"))?;
    let admin = pt.obj(ObjectArg::ImmOrOwnedObject((
        admin_ref.object_id,
        admin_ref.version,
        admin_ref.digest,
    )))?;

    let ireg = pt.obj(shared_mut_arg(client, objects.integration_registry_id).await?)?;
    let oreg = pt.obj(shared_mut_arg(client, objects.oracle_registry_id).await?)?;
    let feed_reg = pt.obj(shared_mut_arg(client, objects.pyth_feed_registry_id).await?)?;

    let type_name_call = |pt: &mut ProgrammableTransactionBuilder, ty: &str| -> Result<_> {
        let tag = TypeTag::from_str(ty).with_context(|| format!("parsing witness type {ty}"))?;
        Ok(pt.programmable_move_call(
            ObjectID::from_hex_literal("0x1")?,
            Identifier::new("type_name")?,
            Identifier::new("with_defining_ids")?,
            vec![tag],
            vec![],
        ))
    };

    // Integration witnesses.
    for witness in [
        format!("{deepbook_adapter_pkg}::deepbook_adapter::DeepBookAdapter"),
        format!("{options_adapter_pkg}::options_adapter::OptionsAdapter"),
        format!("{trading_vault_pkg}::vault_mm::VaultMm"),
    ] {
        let t = type_name_call(&mut pt, &witness)?;
        pt.programmable_move_call(
            trading_vault_pkg,
            Identifier::new("registry")?,
            Identifier::new("allow_adapter")?,
            vec![],
            vec![admin, ireg, t],
        );
    }
    // Oracle witnesses: Pyth for catalog assets, the options intrinsic
    // oracle for per-bucket option coins (SO-297).
    for witness in [
        format!("{oracle_pyth_pkg}::oracle_pyth::PythOracle"),
        format!("{options_adapter_pkg}::options_oracle::OptionsOracle"),
    ] {
        let t = type_name_call(&mut pt, &witness)?;
        pt.programmable_move_call(
            trading_vault_pkg,
            Identifier::new("registry")?,
            Identifier::new("allow_oracle")?,
            vec![],
            vec![admin, oreg, t],
        );
    }

    // Feed seeding from the token catalog (skip feed-less tokens).
    let mut seeded = 0usize;
    for (symbol, spec) in token_info {
        let Some(feed) = spec.pyth_feed_id.as_deref() else {
            continue;
        };
        let bytes = hex::decode(feed.trim_start_matches("0x"))
            .with_context(|| format!("decoding feed id for {symbol}"))?;
        let coin_type = TypeTag::from_str(&spec.coin_type)
            .with_context(|| format!("parsing coin type for {symbol}"))?;
        let feed_arg = pt.pure(bytes)?;
        let dec_arg = pt.pure(spec.decimals)?;
        pt.programmable_move_call(
            oracle_pyth_pkg,
            Identifier::new("oracle_pyth")?,
            Identifier::new("set_feed")?,
            vec![coin_type],
            vec![admin, feed_reg, feed_arg, dec_arg],
        );
        seeded += 1;
    }

    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching gas price")?;
    let coins = client
        .coin_read_api()
        .get_coins(signer.address, None, None, None)
        .await
        .context("fetching gas coins")?;
    let gas = coins
        .data
        .iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no gas coins for activation tx"))?;

    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas.object_ref()],
        pt.finish(),
        gas_budget,
        gas_price,
    );
    let signature = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![signature]);
    tracing::info!(feeds = seeded, "submitting trading-vault activation tx");
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            SuiTransactionBlockResponseOptions::new().with_effects(),
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting activation tx")?;
    assert_success(&resp)?;
    Ok(resp.digest.to_string())
}
