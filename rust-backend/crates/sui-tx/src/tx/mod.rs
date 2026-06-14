//! PTB builders for the protocol's entry points.
//!
//! Two layers here:
//!
//! - Simple Move calls (admin operations, account create) use the high-level
//!   `client.transaction_builder().move_call(...)` API. Everything's a
//!   primitive or an object id, so JSON-encoded args work fine. See
//!   [`admin`] and [`account`].
//!
//! - `execute_write` needs to splice a fresh `SignedQuote` value (built from
//!   `quote::new_quote` + `quote::new_signed_quote`), split a coin from gas,
//!   call `coin::zero<S>` for the empty side, and pass a `FlowKind` enum —
//!   none of that fits the high-level builder. We drop down to
//!   `ProgrammableTransactionBuilder` for that one. See [`execute_write`].

pub mod account;
pub mod admin;
pub mod coin_pkg;
pub mod deepbook;
pub mod execute_write;
pub mod pyth_update;
pub mod rfq;
pub mod sponsor;
pub mod swap_auction;
pub mod template;
pub mod test_tokens;
pub mod vault;
pub mod vault_create;

use anyhow::{anyhow, Context, Result};
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_types::base_types::ObjectID;
use sui_types::object::Owner;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use tracing::{debug, trace};

use sui_sdk::SuiClient;
use sui_json_rpc_types::SuiObjectDataOptions;

use crate::sui_client::Signer;

/// Build a `SharedObject` `ObjectArg` from a current chain read. Needed by
/// PTBs that mutate shared objects (Bucket, ProtocolConfig, Treasury,
/// Account, Clock).
pub async fn shared_object_arg(
    client: &SuiClient,
    id: ObjectID,
    mutable: bool,
) -> Result<ObjectArg> {
    trace!(%id, mutable, "fetching shared object arg");
    let resp = client
        .read_api()
        .get_object_with_options(id, SuiObjectDataOptions::new().with_owner())
        .await?;
    let data = resp
        .data
        .ok_or_else(|| anyhow!("object {id} not found on chain"))?;
    let owner = data
        .owner
        .ok_or_else(|| anyhow!("object {id} has no owner field"))?;
    match owner {
        Owner::Shared {
            initial_shared_version,
        } => Ok(ObjectArg::SharedObject {
            id,
            initial_shared_version,
            mutability: if mutable {
                SharedObjectMutability::Mutable
            } else {
                SharedObjectMutability::Immutable
            },
        }),
        other => Err(anyhow!("object {id} is not shared: {:?}", other)),
    }
}

/// Gas-select, sign, submit, and assert success for a finished PTB. Shared
/// by the rfq / vault builders (and the keeper, which prepends a Pyth
/// price update before the crank call); the older modules keep their
/// local copies.
pub async fn submit_ptb(
    client: &SuiClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
    label: &str,
) -> Result<SuiTransactionBlockResponse> {
    let programmable = pt.finish();

    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(10))
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
        .with_context(|| format!("submitting {label} tx"))?;
    let effects = resp.effects.as_ref().context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("{label} reverted: {:?}", effects.status());
    }
    debug!(digest = %resp.digest, label, "tx succeeded");
    Ok(resp)
}

/// Build an owned-object `ObjectArg` (e.g. an AdminCap held by the deployer).
pub async fn owned_object_arg(client: &SuiClient, id: ObjectID) -> Result<ObjectArg> {
    debug!(%id, "fetching owned object arg");
    let resp = client
        .read_api()
        .get_object_with_options(
            id,
            SuiObjectDataOptions::new().with_owner().with_bcs(),
        )
        .await?;
    let data = resp.data.ok_or_else(|| anyhow!("object {id} not found"))?;
    Ok(ObjectArg::ImmOrOwnedObject(data.object_ref()))
}
