//! Admin-cap-gated PTBs: `new_call_option`, `set_fee_bps`, `withdraw_treasury`.
//!
//! All three are simple Move calls — no coin manipulation — so they go
//! through the high-level `client.transaction_builder().move_call(...)`
//! builder. The same builder auto-selects a gas coin and computes the
//! reference gas price.

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
    Ok(resp)
}

pub struct NewCallOptionArgs<'a> {
    pub package: ObjectID,
    pub admin_cap: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub expiry_ms: u64,
    pub start_strike: u64,
    pub strike_interval: u64,
    pub count: u64,
}

/// Calls `bucket::new_call_option<U, S>(&AdminCap, expiry, start, interval,
/// count, ctx)`. Emits one `BucketCreated` event per strike.
pub async fn new_call_option(
    client: &SuiClient,
    signer: &Signer,
    args: &NewCallOptionArgs<'_>,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    execute_move_call(
        client,
        signer,
        args.package,
        "bucket",
        "new_call_option",
        vec![args.underlying_type, args.settlement_type],
        vec![
            SuiJsonValue::from_object_id(args.admin_cap),
            // u64 args ride as string in JSON to avoid 2^53 truncation.
            SuiJsonValue::new(serde_json::Value::String(args.expiry_ms.to_string()))?,
            SuiJsonValue::new(serde_json::Value::String(args.start_strike.to_string()))?,
            SuiJsonValue::new(serde_json::Value::String(args.strike_interval.to_string()))?,
            SuiJsonValue::new(serde_json::Value::String(args.count.to_string()))?,
        ],
        gas_budget,
    )
    .await
}

/// Calls `admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)`.
pub async fn set_fee_bps(
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
        "set_fee_bps",
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

/// Calls `treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient,
/// ctx)`.
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
    execute_move_call(
        client,
        signer,
        package,
        "treasury",
        "withdraw",
        vec![asset_type],
        vec![
            SuiJsonValue::from_object_id(admin_cap),
            SuiJsonValue::from_object_id(treasury),
            SuiJsonValue::new(serde_json::Value::String(amount.to_string()))?,
            SuiJsonValue::new(serde_json::Value::String(recipient.to_string()))?,
        ],
        gas_budget,
    )
    .await
}
