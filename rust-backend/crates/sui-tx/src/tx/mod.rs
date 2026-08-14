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
pub mod exchange;
pub mod execute_write;
pub mod execute_write_put;
pub mod funding;
pub mod mm_collateral;
pub mod option_coin;
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

use anyhow::{bail, Context, Result};
use shared_crypto::intent::Intent;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, Command, ObjectArg, ProgrammableTransaction, SharedObjectMutability, Transaction,
    TransactionData,
};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::{debug, trace};

use crate::chain::{ChainClient, ExecutedTransaction, GasPayment};
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

/// Build `TransactionData` that pays for itself out of whatever `sender`
/// actually holds — coin objects when it has them, otherwise its address
/// balance.
///
/// Address-balance gas is not just a different payment list. There is no gas
/// object, so the version bump of a gas coin no longer makes each transaction
/// unique, and the protocol replaces it with a `ValidDuring` expiration: this
/// epoch and the next, bound to this chain, carrying a nonce. We mint a fresh
/// nonce per build, which is also what makes a rebuild-and-retry produce a
/// distinct transaction.
///
/// The same absence forbids `Argument::GasCoin` — there is no gas coin to
/// borrow, split or merge. We check for it here rather than letting the
/// validator reject the transaction, because the error it returns names
/// neither the command nor the reason.
pub async fn gas_tx_data(
    client: &ChainClient,
    sender: SuiAddress,
    programmable: ProgrammableTransaction,
    gas_budget: u64,
) -> Result<TransactionData> {
    let payment = client.gas_payment(sender, gas_budget).await?;
    let gas_price = client
        .reference_gas_price()
        .await
        .context("fetching reference gas price")?;

    match payment {
        GasPayment::Coins(coins) => Ok(TransactionData::new_programmable(
            sender,
            coins,
            programmable,
            gas_budget,
            gas_price,
        )),
        GasPayment::AddressBalance => {
            if programmable.commands.iter().any(Command::is_gas_coin_used) {
                bail!(
                    "this PTB borrows Argument::GasCoin, which needs a gas coin object, but \
                     {sender} holds its SUI as an address balance — fund the coin argument with \
                     sui_tx::tx::funding instead"
                );
            }
            let (chain, epoch) = tokio::try_join!(client.chain_id(), client.current_epoch())
                .context("reading chain id / epoch for an address-balance gas payment")?;
            debug!(%sender, gas_budget, "paying gas from the address balance");
            Ok(TransactionData::new_programmable_with_address_balance_gas(
                sender,
                programmable,
                gas_budget,
                gas_price,
                chain,
                epoch,
                rand::random(),
            ))
        }
    }
}

/// Attempts (including the first) `submit_ptb` makes when the gas coin
/// reference it built with turns out to be stale. Waits between attempts are
/// 300ms / 900ms / 2.7s — the observed read lag is sub-second, so this is
/// several times the window that actually needs covering.
const GAS_REF_ATTEMPTS: u32 = 4;

/// Did the node reject this transaction because the gas coin reference was
/// not the current version?
///
/// This is a *rejection*, not a revert: validators refuse to admit the
/// transaction at all, so nothing executed and rebuilding with a fresh gas
/// reference is safe — the new reference yields a different digest, so the
/// resubmission cannot double-apply the effects of the original.
///
/// The staleness is normal under gRPC. `ExecuteTransaction` returns once
/// validators finalize, but the fullnode's *read* view can still be a version
/// behind, so a transaction built moments after another one from the same
/// address selects the pre-tx gas reference. JSON-RPC's
/// `WaitForLocalExecution` used to hide this, which is why it only appeared
/// after the gRPC migration (SO-337): mm-bot's bootstrap creates its quote
/// signer and then immediately funds its collateral account, and the second
/// transaction was rejected on every boot (SO-343).
///
/// Public because callers that own durable state past the retry budget need
/// the same answer: the option-scheduler classifies a submit failure to decide
/// whether its claimed roll slot can be released (SO-344).
pub fn is_stale_gas_rejection(err: &anyhow::Error) -> bool {
    // Matched on the message because the gRPC status is flattened into an
    // anyhow chain by ChainClient::execute. Both phrasings come from the same
    // validator rejection; either alone is sufficient.
    let msg = format!("{err:#}");
    msg.contains("is unavailable for consumption")
        || msg.contains("needs to be rebuilt because object")
}

/// Gas-select, sign, submit, and assert success for a finished PTB. Shared
/// by the rfq / vault builders (and the keeper, which prepends a Pyth
/// price update before the crank call); the older modules keep their
/// local copies.
///
/// Re-reads the gas coin and resubmits when the node rejects the transaction
/// for a stale gas reference — see [`is_stale_gas_rejection`]. Only that one
/// rejection is retried; Move aborts and transport errors propagate on the
/// first failure, since those may have executed.
///
/// Takes an already-finished PTB, so only the *gas* reference is refreshed
/// between attempts. If the transaction also consumes an owned object the
/// sender mutates elsewhere — an AdminCap, a TreasuryCap — that input's
/// reference is baked in and every retry fails identically; use
/// [`submit_ptb_rebuilding`] instead.
pub async fn submit_ptb(
    client: &ChainClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
    label: &str,
) -> Result<ExecutedTransaction> {
    let programmable = pt.finish();
    submit_ptb_rebuilding(client, signer, gas_budget, label, || {
        let programmable = programmable.clone();
        async move { Ok(programmable) }
    })
    .await
}

/// [`submit_ptb`] for transactions whose *inputs* can also go stale.
///
/// `build` is called once per attempt and must re-read every object reference
/// it embeds, so a retry picks up the current version of the AdminCap (or any
/// other owned input) as well as the gas coin. Rebuilding is what makes the
/// retry meaningful: re-selecting gas alone leaves the stale input in place,
/// which is how the option-scheduler's post-roll `allow_pool` call failed on
/// all four attempts — the roll's own `create_buckets` had just bumped the
/// AdminCap it was still referencing (SO-344).
///
/// Same safety argument as [`is_stale_gas_rejection`]: this is a rejection, so
/// nothing executed, and a rebuilt transaction has a fresh digest.
pub async fn submit_ptb_rebuilding<F, Fut>(
    client: &ChainClient,
    signer: &Signer,
    gas_budget: u64,
    label: &str,
    build: F,
) -> Result<ExecutedTransaction>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<ProgrammableTransaction>>,
{
    for attempt in 1..=GAS_REF_ATTEMPTS {
        // Re-read per attempt: a stale reference is exactly what we are
        // recovering from, so reusing the previous read would spin forever.
        let programmable = build().await.context("building the transaction")?;
        let tx_data = gas_tx_data(client, signer.address, programmable, gas_budget).await?;
        match submit_tx_data(client, signer, tx_data, label).await {
            Ok(resp) => return Ok(resp),
            Err(e) if attempt < GAS_REF_ATTEMPTS && is_stale_gas_rejection(&e) => {
                let backoff =
                    std::time::Duration::from_millis(300 * 3u64.pow(attempt - 1));
                debug!(
                    label,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "object reference stale; rebuilding and resubmitting"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on the final attempt")
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

#[cfg(test)]
mod tests {
    use super::is_stale_gas_rejection;

    /// Verbatim from the rejection that crash-looped mm-bot's bootstrap on
    /// staging (SO-343), truncated after the validator-key list.
    const STALE_GAS: &str = "gRPC ExecuteTransaction: code: 'Client specified an \
invalid argument', message: \"Transaction is rejected as invalid by more than 1/3 \
of validators by stake (non-retriable). Non-retriable errors: [Transaction needs \
to be rebuilt because object 0x49f13ae28ff9bc7e4e4fb8b9a2562465e115f5064b009574ee\
562a6d6225fa87 version 0x396fa533 (68vbhwP1HMbRT145vLywNH65645gcDA2PVnQVDcVEpVj) \
is unavailable for consumption, current version: 0x396fa534 { k#80000033.. } with \
3578 stake].\"";

    #[test]
    fn classifies_the_stale_gas_rejection() {
        assert!(is_stale_gas_rejection(&anyhow::anyhow!("{STALE_GAS}")));
    }

    #[test]
    fn classifies_through_a_context_chain() {
        // submit_tx_data wraps the gRPC error in `submitting {label} tx`, so
        // the marker is only visible with the `{:#}` (full-chain) formatting
        // the classifier uses.
        let err = anyhow::anyhow!("{STALE_GAS}");
        let wrapped = err.context("submitting test token tx");
        assert!(is_stale_gas_rejection(&wrapped));
    }

    /// A Move abort must never be retried — it executed and reverted.
    #[test]
    fn does_not_classify_a_move_abort() {
        let err = anyhow::anyhow!(
            "execute_write reverted: Failure {{ error: MoveAbort(MoveLocation \
{{ module: ModuleId {{ address: 0x5040, name: Identifier(\"mm_collateral\") }}, \
function: 3, instruction: 21 }}, 31) }}"
        );
        assert!(!is_stale_gas_rejection(&err));
    }

    /// Transport failures may have executed; they must propagate, not retry.
    #[test]
    fn does_not_classify_a_transport_error() {
        let err = anyhow::anyhow!(
            "gRPC ExecuteTransaction: status: Unavailable, message: \"error trying \
to connect: tcp connect error: Connection refused (os error 111)\""
        );
        assert!(!is_stale_gas_rejection(&err));
    }

    /// Insufficient gas is a real, terminal condition — retrying just burns
    /// the attempt budget and hides the cause.
    #[test]
    fn does_not_classify_insufficient_gas() {
        let err = anyhow::anyhow!(
            "gRPC ExecuteTransaction: code: 'Client specified an invalid argument', \
message: \"Balance of gas object 10 is lower than the needed amount: 1000000\""
        );
        assert!(!is_stale_gas_rejection(&err));
    }
}
