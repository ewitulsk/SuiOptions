//! Admin-cap-gated PTBs: `new_call_option`, `set_fee_bps`, `withdraw_treasury`.
//!
//! All three are simple Move calls — no coin manipulation. They used to go
//! through the JSON-RPC `transaction_builder().move_call(..)` helper, which
//! resolved object args and pure args from JSON. That builder only exists on
//! the retired JSON-RPC client, so the calls are now assembled explicitly:
//! `ChainClient::object_arg` does the shared-vs-owned resolution the old
//! builder did, and pure args are BCS-encoded directly.

use anyhow::{Context, Result};
use std::str::FromStr;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Argument;
use sui_types::{Identifier, TypeTag};
use tracing::info;

use crate::chain::{ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;
use super::submit_ptb;

/// One argument to an admin Move call: either an on-chain object (resolved
/// by reading its owner) or a pure BCS value.
pub enum CallArg {
    /// Object id + whether the call takes it by `&mut`.
    Object(ObjectID, bool),
    Pure(Vec<u8>),
}

/// Build, sign, submit, and wait for the on-chain effects of a Move call.
async fn execute_move_call(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    module: &'static str,
    function: &'static str,
    type_args: Vec<&str>,
    args: Vec<CallArg>,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    info!(%package, module, function, gas_budget, "submitting move call");
    let type_arguments: Vec<TypeTag> = type_args
        .into_iter()
        .map(|s| TypeTag::from_str(s).with_context(|| format!("parsing type tag {s}")))
        .collect::<Result<_>>()?;

    let mut pt = ProgrammableTransactionBuilder::new();
    let mut arguments: Vec<Argument> = Vec::with_capacity(args.len());
    for arg in args {
        let a = match arg {
            CallArg::Object(id, mutable) => {
                let oa = client
                    .object_arg(id, mutable)
                    .await
                    .with_context(|| format!("resolving object arg {id}"))?;
                pt.obj(oa)?
            }
            CallArg::Pure(bytes) => pt.pure_bytes(bytes, /* force_separate */ false),
        };
        arguments.push(a);
    }
    pt.programmable_move_call(
        package,
        Identifier::new(module)?,
        Identifier::new(function)?,
        type_arguments,
        arguments,
    );

    submit_ptb(client, signer, pt, gas_budget, &format!("{module}::{function}")).await
}

/// Calls `admin::set_fee_bps(&AdminCap, &mut ProtocolConfig, new_bps)`.
pub async fn set_fee_bps(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    protocol_config: ObjectID,
    new_bps: u64,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    execute_move_call(
        client,
        signer,
        package,
        "admin",
        "set_fee_bps",
        vec![],
        vec![
            CallArg::Object(admin_cap, false),
            CallArg::Object(protocol_config, true),
            CallArg::Pure(bcs::to_bytes(&new_bps)?),
        ],
        gas_budget,
    )
    .await
}

/// Calls `treasury::withdraw<T>(&AdminCap, &mut Treasury, amount, recipient,
/// ctx)`.
pub async fn withdraw_treasury(
    client: &ChainClient,
    signer: &Signer,
    package: ObjectID,
    admin_cap: ObjectID,
    treasury: ObjectID,
    asset_type: &str,
    amount: u64,
    recipient: SuiAddress,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    execute_move_call(
        client,
        signer,
        package,
        "treasury",
        "withdraw",
        vec![asset_type],
        vec![
            CallArg::Object(admin_cap, false),
            CallArg::Object(treasury, true),
            CallArg::Pure(bcs::to_bytes(&amount)?),
            CallArg::Pure(bcs::to_bytes(&recipient)?),
        ],
        gas_budget,
    )
    .await
}
