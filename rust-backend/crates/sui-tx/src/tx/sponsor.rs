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
use sui_types::base_types::{ObjectRef, SuiAddress};
use sui_types::crypto::EncodeDecodeBase64;
use sui_types::transaction::{Command, GasData, Transaction, TransactionData, TransactionKind};
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::template::{describe_ptb, match_any, PtbTemplate};
use crate::chain::ChainClient;

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
    client: &ChainClient,
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
    // `ChainClient::coins` already sorts by balance descending.
    let coins = client
        .coins(sponsor, &crate::chain::sui_coin_type())
        .await
        .context("listing sponsor gas coins")?;
    if coins.is_empty() {
        bail!("gas station wallet {sponsor} has no SUI coins");
    }
    let mut payment: Vec<ObjectRef> = Vec::new();
    let mut available: u64 = 0;
    for c in &coins {
        payment.push(c.object_ref);
        available = available.saturating_add(c.balance);
        if available >= policy.max_gas_budget {
            break;
        }
    }

    let gas_price = client
        .reference_gas_price()
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
        .dry_run(&probe)
        .await
        .context("dry-running sponsored transaction")?;
    let dry_effects = &dry.transaction.effects;
    {
        use sui_types::effects::TransactionEffectsAPI;
        let status = dry_effects.status();
        if status.is_err() {
            bail!("transaction would fail on chain: {status:?}");
        }
    }

    // Budget = (computation + storage) + buffer, clamped to [min, max].
    use sui_types::effects::TransactionEffectsAPI;
    let summary = dry_effects.gas_cost_summary();
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

/// Total SUI balance (MIST) of the gas station wallet — coin objects *and*
/// address balance, since [`top_up_gas_coins`] can turn the latter into the
/// former.
pub async fn sponsor_balance(client: &ChainClient, sponsor: SuiAddress) -> Result<u128> {
    client
        .balance(sponsor, &crate::chain::sui_coin_type())
        .await
        .context("fetching sponsor balance")
}

/// Move address balance into a coin object so the sponsor can keep sponsoring.
///
/// Unlike every other submission in this workspace, sponsorship cannot spend an
/// address balance directly: `GasData.payment` must name the sponsor's coins,
/// and the *user's* wallet signs those exact bytes. A sponsor topped up by
/// faucet or transfer therefore ends up rich and unable to sponsor anything —
/// [`sponsor_transaction`] bails with "has no SUI coins".
///
/// So we materialise one. The redeem transaction itself is free of the problem
/// it fixes: it pays its own gas from the address balance
/// ([`crate::tx::gas_tx_data`]), which is why this can bootstrap a sponsor that
/// owns nothing at all.
///
/// Returns the digest when it topped up, `None` when there was nothing to do:
/// coins already cover `target`, or the address balance has nothing to spare
/// beyond `gas_budget` (kept back so the redeem can pay for itself).
pub async fn top_up_gas_coins(
    client: &ChainClient,
    signer: &Signer,
    target: u64,
    gas_budget: u64,
) -> Result<Option<String>> {
    let sui = crate::chain::sui_coin_type();
    let coins = client
        .coins(signer.address, &sui)
        .await
        .context("listing sponsor gas coins")?;
    let in_coins = coins
        .iter()
        .map(|c| c.balance)
        .fold(0u64, |a, b| a.saturating_add(b));
    if in_coins >= target {
        return Ok(None);
    }

    let available = client
        .address_balance(signer.address, &sui)
        .await
        .context("reading the sponsor's address balance")?;
    let amount = available
        .saturating_sub(gas_budget)
        .min(target.saturating_sub(in_coins));
    if amount == 0 {
        return Ok(None);
    }

    let pt = crate::tx::funding::redeem_to_coin(signer.address, &sui, amount)?;
    let tx_data = crate::tx::gas_tx_data(client, signer.address, pt, gas_budget).await?;
    let resp = crate::tx::submit_tx_data(client, signer, tx_data, "sponsor gas top-up").await?;
    let digest = crate::tx::tx_digest(&resp).to_string();
    info!(
        sponsor = %signer.address, amount, in_coins, available, %digest,
        "moved address balance into a sponsor gas coin"
    );
    Ok(Some(digest))
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
            protocol_templates(ObjectID::random(), Some(ObjectID::random()), &[], false, None, None, None);
        let kind = movecall_kind(ObjectID::random());
        let err = validate_kind(&kind, &templates).unwrap_err();
        assert!(err.to_string().contains("no sponsored template"), "{err}");
    }
}
