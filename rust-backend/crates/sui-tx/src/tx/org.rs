//! Org-cap-gated PTBs: `create_org`, `set_org_fee_bps`, `withdraw_org`.
//!
//! Orgs are permissionless: anyone can call `org::create_org`. The cap is
//! RETURNED by the contract, so `create_org` builds a raw PTB that transfers
//! it to the signer; likewise `org::withdraw` returns the coin.

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use shared_crypto::intent::Intent;
use std::str::FromStr;
use sui_json_rpc_types::{
    ObjectChange, SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Transaction, TransactionData};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::TypeTag;
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::{owned_object_arg, shared_object_arg};

pub struct CreateOrgOutcome {
    /// The shared `Org` object.
    pub org_id: ObjectID,
    /// The `OrgCap` transferred to the signer.
    pub org_cap_id: ObjectID,
    pub digest: String,
}

/// `org::create_org(name, fee_bps): OrgCap` — shares the Org and transfers
/// the returned cap to the signer.
pub async fn create_org(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    name: &str,
    fee_bps: u64,
    gas_budget: u64,
) -> Result<CreateOrgOutcome> {
    info!(%package, name, fee_bps, "building create_org PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let name_arg = pt.pure(name.to_string())?;
    let fee_arg = pt.pure(fee_bps)?;
    let cap = pt.programmable_move_call(
        package,
        Identifier::new("org").unwrap(),
        Identifier::new("create_org").unwrap(),
        vec![],
        vec![name_arg, fee_arg],
    );
    pt.transfer_args(signer.address, vec![cap]);

    let resp = submit(client, signer, pt, gas_budget, "create_org").await?;
    let digest = resp.digest.to_string();
    let changes = resp
        .object_changes
        .as_ref()
        .ok_or_else(|| anyhow!("create_org response missing object_changes"))?;
    let mut org_id = None;
    let mut org_cap_id = None;
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
    Ok(CreateOrgOutcome {
        org_id: org_id.ok_or_else(|| anyhow!("Org not found in create_org changes"))?,
        org_cap_id: org_cap_id.ok_or_else(|| anyhow!("OrgCap not found in create_org changes"))?,
        digest,
    })
}

/// `org::set_org_fee_bps(&OrgCap, &mut Org, new_bps)`.
pub async fn set_org_fee_bps(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    org_cap: ObjectID,
    org: ObjectID,
    new_bps: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(%package, %org, new_bps, "building set_org_fee_bps PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap_arg = pt.obj(owned_object_arg(client, org_cap).await?)?;
    let org_arg = pt.obj(shared_object_arg(client, org, true).await?)?;
    let bps_arg = pt.pure(new_bps)?;
    pt.programmable_move_call(
        package,
        Identifier::new("org").unwrap(),
        Identifier::new("set_org_fee_bps").unwrap(),
        vec![],
        vec![cap_arg, org_arg, bps_arg],
    );
    submit(client, signer, pt, gas_budget, "set_org_fee_bps").await
}

/// `org::withdraw<T>(&OrgCap, &mut Org, amount, ctx): Coin<T>` — the coin is
/// returned; the PTB transfers it to `recipient`.
pub async fn withdraw_org(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    org_cap: ObjectID,
    org: ObjectID,
    asset_type: &str,
    amount: u64,
    recipient: SuiAddress,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(%package, %org, asset_type, amount, %recipient, "building org withdraw PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap_arg = pt.obj(owned_object_arg(client, org_cap).await?)?;
    let org_arg = pt.obj(shared_object_arg(client, org, true).await?)?;
    let amount_arg = pt.pure(amount)?;
    let t_tag = TypeTag::from_str(asset_type)
        .with_context(|| format!("parsing asset type {asset_type}"))?;
    let coin = pt.programmable_move_call(
        package,
        Identifier::new("org").unwrap(),
        Identifier::new("withdraw").unwrap(),
        vec![t_tag],
        vec![cap_arg, org_arg, amount_arg],
    );
    pt.transfer_args(recipient, vec![coin]);
    submit(client, signer, pt, gas_budget, "org withdraw").await
}

/// Gas-select, sign, submit, and assert success.
async fn submit(
    client: &SuiClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
    what: &str,
) -> Result<SuiTransactionBlockResponse> {
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
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .with_context(|| format!("submitting {what} tx"))?;
    let effects = resp.effects.as_ref().context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("{what} reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, what, "tx succeeded");
    Ok(resp)
}
