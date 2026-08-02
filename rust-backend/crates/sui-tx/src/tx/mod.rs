//! PTB builders for the protocol's entry points.
//!
//! Two layers here:
//!
//! - Simple Move calls (admin operations, quote-signer create) use the
//!   high-level `client.transaction_builder().move_call(...)` API.
//!   Everything's a primitive or an object id, so JSON-encoded args work
//!   fine. See [`admin`] and [`signer`].
//!
//! - `execute_write` needs to splice a fresh `SignedQuote` value (built from
//!   `quote::new_quote` + `quote::new_signed_quote`), mint the potato via
//!   `request_*_flow`, route the MM-specified `release` call, and consume
//!   both in `execute_*_flow` — none of that fits the high-level builder. We
//!   drop down to `ProgrammableTransactionBuilder` for that one. See
//!   [`execute_write`].

pub mod admin;
pub mod auction;
pub mod coin_pkg;
pub mod deepbook;
pub mod execute_write;
pub mod execute_write_put;
pub mod mm_collateral;
pub mod oracle;
pub mod pyth_update;
pub mod signer;
pub mod sponsor;
pub mod template;
pub mod test_tokens;
pub mod appraisal;
pub mod trading_vault;
pub mod vault;
pub mod vault_create;

use anyhow::{Context, Result};
use shared_crypto::intent::Intent;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::{debug, trace};

use crate::chain::{ChainClient, ExecutedTransaction};
use crate::sui_client::Signer;

/// Build a `SharedObject` `ObjectArg` from a current chain read. Needed by
/// PTBs that mutate shared objects (Bucket, ProtocolConfig, Treasury,
/// Account, Clock).
pub async fn shared_object_arg(
    client: &ChainClient,
    id: ObjectID,
    mutable: bool,
) -> Result<ObjectArg> {
    trace!(%id, mutable, "fetching shared object arg");
    client.shared_object_arg(id, mutable).await
}

/// Immutable Clock argument, shared by every deadline-aware builder.
pub fn clock_arg(pt: &mut ProgrammableTransactionBuilder) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

/// Gas-select, sign, submit, and assert success for a finished PTB. Shared
/// by the rfq / vault builders (and the keeper, which prepends a Pyth
/// price update before the crank call); the older modules keep their
/// local copies.
pub async fn submit_ptb(
    client: &ChainClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
    label: &str,
) -> Result<ExecutedTransaction> {
    let programmable = pt.finish();

    let gas_coin = client
        .gas_coin(signer.address)
        .await
        .context("selecting a gas coin")?;
    let gas_price = client
        .reference_gas_price()
        .await
        .context("fetching reference gas price")?;

    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin],
        programmable,
        gas_budget,
        gas_price,
    );
    submit_tx_data(client, signer, tx_data, label).await
}

/// Sign, submit, and assert success for a fully-formed `TransactionData`.
/// Split out of [`submit_ptb`] so callers that build their own gas payment
/// (sponsored txs, coin-specific gas) share the signing and status check.
pub async fn submit_tx_data(
    client: &ChainClient,
    signer: &Signer,
    tx_data: TransactionData,
    label: &str,
) -> Result<ExecutedTransaction> {
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let resp = client
        .execute(&tx)
        .await
        .with_context(|| format!("submitting {label} tx"))?;
    assert_success(&resp, label)?;
    debug!(digest = %tx_digest(&resp), label, "tx succeeded");
    Ok(resp)
}

/// Bail with the Move abort / execution status when a transaction reverted.
/// `clever_error` carries the decoded `#[error]` constant when the package
/// ships one, so surface it — it is the difference between "abort 31" and a
/// named reason.
pub fn assert_success(resp: &ExecutedTransaction, label: &str) -> Result<()> {
    use sui_types::effects::TransactionEffectsAPI;
    let status = resp.effects.status();
    if status.is_err() {
        match &resp.clever_error {
            Some(ce) => anyhow::bail!("{label} reverted: {status:?} ({ce:?})"),
            None => anyhow::bail!("{label} reverted: {status:?}"),
        }
    }
    Ok(())
}

/// Digest of an executed transaction — the `resp.digest` of the old
/// JSON-RPC response.
pub fn tx_digest(resp: &ExecutedTransaction) -> sui_types::digests::TransactionDigest {
    use sui_types::effects::TransactionEffectsAPI;
    *resp.effects.transaction_digest()
}

/// Build an owned-object `ObjectArg` (e.g. an AdminCap held by the deployer).
pub async fn owned_object_arg(client: &ChainClient, id: ObjectID) -> Result<ObjectArg> {
    debug!(%id, "fetching owned object arg");
    client.owned_object_arg(id).await
}
