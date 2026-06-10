//! Admin-cap-gated PTBs: `set_protocol_fee_bps`, `set_pause`,
//! `withdraw_treasury`.
//!
//! The fee/pause setters are simple Move calls and go through the high-level
//! `client.transaction_builder().move_call(...)` builder. `withdraw_treasury`
//! drops to a raw PTB because `treasury::withdraw` now RETURNS the coin —
//! the PTB must transfer it to the recipient itself.

use anyhow::{Context, Result};
use shared_crypto::intent::Intent;
use std::str::FromStr;
use sui_json::SuiJsonValue;
use sui_json_rpc_types::{
    SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions, SuiTypeTag,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::transaction::Transaction;
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::TypeTag;
use tracing::{debug, info};

use crate::sui_client::Signer;

/// Build, sign, submit, and wait for the on-chain effects of a Move call.
async fn execute_move_call(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    module: &'static str,
    function: &'static str,
    type_args: Vec<&str>,
    args: Vec<SuiJsonValue>,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    info!(%package, module, function, gas_budget, "submitting move call");
    let type_args: Vec<SuiTypeTag> = type_args
        .into_iter()
        .map(|s| {
            TypeTag::from_str(s)
                .with_context(|| format!("parsing type tag {s}"))
                .map(SuiTypeTag::from)
        })
        .collect::<Result<_>>()?;
    let tx_data = client
        .transaction_builder()
        .move_call(
            signer.address,
            package,
            module,
            function,
            type_args,
            args,
            None,
            gas_budget,
            None,
        )
        .await
        .with_context(|| format!("building {module}::{function} tx"))?;
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
        .with_context(|| format!("submitting {module}::{function} tx"))?;
    let effects = resp
        .effects
        .as_ref()
        .context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("{module}::{function} reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, module, function, "move call succeeded");
    Ok(resp)
}

/// Calls `admin::set_protocol_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)`.
pub async fn set_protocol_fee_bps(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    protocol_config: ObjectID,
    new_bps: u64,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    execute_move_call(
        client,
        signer,
        package,
        "admin",
        "set_protocol_fee_bps",
        vec![],
        vec![
            SuiJsonValue::from_object_id(admin_cap),
            SuiJsonValue::from_object_id(protocol_config),
            SuiJsonValue::new(serde_json::Value::String(new_bps.to_string()))?,
        ],
        gas_budget,
    )
    .await
}

/// Calls `admin::set_pause(&AdminCap, &mut ProtocolConfig, paused, ctx)` —
/// the protocol-wide emergency brake on new writes.
pub async fn set_pause(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    protocol_config: ObjectID,
    paused: bool,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    execute_move_call(
        client,
        signer,
        package,
        "admin",
        "set_pause",
        vec![],
        vec![
            SuiJsonValue::from_object_id(admin_cap),
            SuiJsonValue::from_object_id(protocol_config),
            SuiJsonValue::new(serde_json::Value::Bool(paused))?,
        ],
        gas_budget,
    )
    .await
}

/// `treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, ctx): Coin<T>` —
/// the coin is RETURNED, so this builds a raw PTB that transfers it to
/// `recipient`.
pub async fn withdraw_treasury(
    client: &SuiClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    treasury: ObjectID,
    asset_type: &str,
    amount: u64,
    recipient: SuiAddress,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    use move_core_types::identifier::Identifier;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
    use sui_types::transaction::TransactionData;
    use sui_types::TypeTag as MoveTypeTag;

    use crate::tx::{owned_object_arg, shared_object_arg};

    info!(%package, asset_type, amount, %recipient, "building treasury withdraw PTB");
    let mut pt = ProgrammableTransactionBuilder::new();
    let cap_arg = pt.obj(owned_object_arg(client, admin_cap).await?)?;
    let treasury_arg = pt.obj(shared_object_arg(client, treasury, true).await?)?;
    let amount_arg = pt.pure(amount)?;
    let t_tag = MoveTypeTag::from_str(asset_type)
        .with_context(|| format!("parsing asset type {asset_type}"))?;
    let coin = pt.programmable_move_call(
        package,
        Identifier::new("treasury").unwrap(),
        Identifier::new("withdraw").unwrap(),
        vec![t_tag],
        vec![cap_arg, treasury_arg, amount_arg],
    );
    pt.transfer_args(recipient, vec![coin]);
    let programmable = pt.finish();

    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow::anyhow!("no SUI coins to pay gas for {}", signer.address))?;
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
        .context("submitting treasury withdraw tx")?;
    let effects = resp.effects.as_ref().context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("treasury::withdraw reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, "treasury withdraw succeeded");
    Ok(resp)
}
