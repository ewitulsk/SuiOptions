//! Sponsored-transaction signing for the gas station.
//!
//! The frontend serializes a `TransactionKind` (via dapp-kit
//! `tx.build({ onlyTransactionKind: true })`) — Sui's `GasLessTransactionData`
//! — and hands it here. We attach the sponsor's `GasData`, dry-run to size the
//! gas budget, and sign the resulting `TransactionData` with the sponsor key.
//! The user's wallet then signs the *same* bytes; both signatures execute
//! together (the dual-signature requirement from Sui's sponsored-tx model).

use anyhow::{bail, Context, Result};
use base64::Engine;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{SuiExecutionStatus, SuiTransactionBlockEffectsAPI};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectRef, SuiAddress};
use sui_types::crypto::EncodeDecodeBase64;
use sui_types::transaction::{Command, GasData, Transaction, TransactionData, TransactionKind};
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::template::{describe_ptb, match_any, PtbTemplate};

/// Budget-sizing knobs (from gas-station config).
pub struct BudgetPolicy {
    /// Hard cap on a sponsored gas budget (MIST). Also the budget used for the
    /// dry run.
    pub max_gas_budget: u64,
    /// Floor for a sponsored gas budget (MIST).
    pub min_gas_budget: u64,
    /// Safety margin added on top of the dry-run estimate, basis points.
    pub buffer_bps: u64,
}

/// Result of sponsoring: the full `TransactionData` (base64 BCS) plus the
/// sponsor's signature over it.
pub struct SponsoredTx {
    /// base64(bcs(TransactionData)) — what the wallet signs and what's
    /// submitted as `transactionBlock`.
    pub tx_bytes_b64: String,
    /// base64 sponsor signature over the same `TransactionData`.
    pub sponsor_signature_b64: String,
    pub gas_budget: u64,
    pub gas_price: u64,
}

/// Reject anything we're not willing to pay for: only programmable
/// transactions, never publish/upgrade, and — when `templates` is non-empty —
/// the PTB must match one of the exact shapes the frontend builds (see
/// [`crate::tx::template`]). An empty template set means "allow all" (local
/// dev).
fn validate_kind(kind: &TransactionKind, templates: &[PtbTemplate]) -> Result<()> {
    let pt = match kind {
        TransactionKind::ProgrammableTransaction(pt) => pt,
        _ => bail!("only programmable transactions can be sponsored"),
    };
    // Never sponsor code deployment, even in local allow-all mode.
    for cmd in &pt.commands {
        if matches!(cmd, Command::Publish(..) | Command::Upgrade(..)) {
            bail!("refusing to sponsor a publish/upgrade transaction");
        }
    }
    if templates.is_empty() {
        return Ok(());
    }
    match match_any(templates, pt) {
        Some(name) => {
            info!(template = name, "PTB matched sponsored template");
            Ok(())
        }
        // Include the decoded command sequence so a refusal can be diffed
        // against the frontend builders without a redeploy (the matcher is
        // otherwise opaque).
        None => bail!("PTB matches no sponsored template: [{}]", describe_ptb(pt)),
    }
}

/// Build, dry-run-size, and sign a sponsored transaction.
///
/// `kind_bytes` is the BCS of a `TransactionKind` (no gas). We never trust a
/// client-supplied budget — the budget is derived from a dry run so the gas
/// wallet can't be made to over-commit.
pub async fn sponsor_transaction(
    client: &SuiClient,
    signer: &Signer,
    templates: &[PtbTemplate],
    policy: &BudgetPolicy,
    sender: SuiAddress,
    kind_bytes: &[u8],
) -> Result<SponsoredTx> {
    let kind: TransactionKind = bcs::from_bytes(kind_bytes)
        .context("decoding TransactionKind (GasLessTransactionData)")?;
    validate_kind(&kind, templates)?;

    let sponsor = signer.address;

    // Sponsor gas coins, largest-first, enough to cover the dry-run budget.
    let mut coins = client
        .coin_read_api()
        .get_coins(sponsor, None, None, Some(50))
        .await
        .context("listing sponsor gas coins")?
        .data;
    if coins.is_empty() {
        bail!("gas station wallet {sponsor} has no SUI coins");
    }
    coins.sort_by(|a, b| b.balance.cmp(&a.balance));
    let mut payment: Vec<ObjectRef> = Vec::new();
    let mut available: u64 = 0;
    for c in &coins {
        payment.push(c.object_ref());
        available = available.saturating_add(c.balance);
        if available >= policy.max_gas_budget {
            break;
        }
    }

    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;

    // Dry-run with the cap as budget (clamped to what the wallet can cover).
    let dry_budget = policy.max_gas_budget.min(available);
    if dry_budget < policy.min_gas_budget {
        bail!(
            "gas station balance too low: {available} MIST across {} coins < min budget {}",
            payment.len(),
            policy.min_gas_budget
        );
    }
    let probe = TransactionData::new_with_gas_data(
        kind.clone(),
        sender,
        GasData {
            payment: payment.clone(),
            owner: sponsor,
            price: gas_price,
            budget: dry_budget,
        },
    );
    let dry = client
        .read_api()
        .dry_run_transaction_block(probe)
        .await
        .context("dry-running sponsored transaction")?;
    if let SuiExecutionStatus::Failure { error } = dry.effects.status() {
        bail!("transaction would fail on chain: {error}");
    }

    // Budget = (computation + storage) + buffer, clamped to [min, max].
    let summary = dry.effects.gas_cost_summary();
    let used = summary
        .computation_cost
        .saturating_add(summary.storage_cost);
    let buffered = used.saturating_add(used.saturating_mul(policy.buffer_bps) / 10_000);
    let budget = buffered.max(policy.min_gas_budget).min(policy.max_gas_budget);
    if budget > available {
        bail!("estimated budget {budget} MIST exceeds gas station balance {available} MIST");
    }

    let tx_data = TransactionData::new_with_gas_data(
        kind,
        sender,
        GasData {
            payment,
            owner: sponsor,
            price: gas_price,
            budget,
        },
    );
    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx_bytes = bcs::to_bytes(&tx_data).context("serializing TransactionData")?;
    let b64 = base64::engine::general_purpose::STANDARD;
    info!(%sender, %sponsor, budget, gas_price, "sponsored transaction signed");
    Ok(SponsoredTx {
        tx_bytes_b64: b64.encode(&tx_bytes),
        sponsor_signature_b64: sig.encode_base64(),
        gas_budget: budget,
        gas_price,
    })
}

/// Total SUI balance (MIST) of the gas station wallet.
pub async fn sponsor_balance(client: &SuiClient, sponsor: SuiAddress) -> Result<u128> {
    let bal = client
        .coin_read_api()
        .get_balance(sponsor, None)
        .await
        .context("fetching sponsor balance")?;
    Ok(bal.total_balance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::template::protocol_templates;
    use move_core_types::identifier::Identifier;
    use sui_types::base_types::ObjectID;
    use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

    fn movecall_kind(pkg: ObjectID) -> TransactionKind {
        let mut pt = ProgrammableTransactionBuilder::new();
        pt.programmable_move_call(
            pkg,
            Identifier::new("m").unwrap(),
            Identifier::new("f").unwrap(),
            vec![],
            vec![],
        );
        TransactionKind::ProgrammableTransaction(pt.finish())
    }

    #[test]
    fn empty_templates_allow_any() {
        let kind = movecall_kind(ObjectID::random());
        assert!(validate_kind(&kind, &[]).is_ok());
    }

    #[test]
    fn unknown_shape_is_rejected() {
        let templates =
            protocol_templates(ObjectID::random(), ObjectID::random(), &[], false, None, None);
        let kind = movecall_kind(ObjectID::random());
        let err = validate_kind(&kind, &templates).unwrap_err();
        assert!(err.to_string().contains("no sponsored template"), "{err}");
    }
}
