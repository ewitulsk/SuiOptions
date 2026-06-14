//! Action → PTB submission, plus the error triage (README §10).
//!
//! Oracle-gated cranks (`select_bucket`, `open_rfq`, `open_swap_rfq`,
//! `settle_swap_rfq`, `finalize_round`) get a fresh Hermes accumulator
//! update prepended in the same PTB (`sui_tx::tx::pyth_update`); the rest
//! call the plain builders.

use anyhow::{anyhow, Context, Result};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use tracing::info;

use pyth_client::types::PriceFeedId;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::pyth_update::{prepend_price_update, PythHandles};
use sui_tx::tx::vault::{PriceInfoRefs, VaultRefs};
use sui_tx::tx::{submit_ptb, vault as vault_tx};

use crate::planner::Action;

/// Everything needed to turn an [`Action`] for one vault into a tx.
pub struct SubmitCtx<'a> {
    pub wrap: &'a SuiClientWrapper,
    pub http: &'a reqwest::Client,
    pub hermes_url: &'a str,
    pub pyth: &'a PythHandles,
    pub package: ObjectID,
    pub protocol_config_id: ObjectID,
    pub treasury_id: ObjectID,
    pub vault_id: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub share_type: &'a str,
    pub underlying_feed: PriceFeedId,
    pub settlement_feed: PriceFeedId,
    pub underlying_price_info: ObjectID,
    pub settlement_price_info: ObjectID,
    pub gas_budget: u64,
}

impl SubmitCtx<'_> {
    fn refs(&self) -> VaultRefs<'_> {
        VaultRefs {
            package: self.package,
            vault_id: self.vault_id,
            underlying_type: self.underlying_type,
            settlement_type: self.settlement_type,
            share_type: self.share_type,
        }
    }

    fn prices(&self) -> PriceInfoRefs {
        PriceInfoRefs {
            underlying_info: self.underlying_price_info,
            settlement_info: self.settlement_price_info,
        }
    }

    /// Start a PTB with the freshest Hermes update for both feeds.
    async fn pt_with_price_update(&self) -> Result<ProgrammableTransactionBuilder> {
        let (payloads, _) = pyth_client::latest_with_update_data(
            self.http,
            self.hermes_url,
            &[self.underlying_feed, self.settlement_feed],
        )
        .await
        .context("fetching hermes update data")?;
        let mut pt = ProgrammableTransactionBuilder::new();
        let infos = [self.underlying_price_info, self.settlement_price_info];
        for payload in &payloads {
            prepend_price_update(&self.wrap.client, &mut pt, self.pyth, payload, &infos)
                .await
                .context("building pyth update prefix")?;
        }
        if payloads.is_empty() {
            return Err(anyhow!("hermes returned no update payloads"));
        }
        Ok(pt)
    }
}

/// Submit one action. The digest is logged by the builders; callers only
/// need the error for triage.
pub async fn execute(ctx: &SubmitCtx<'_>, action: &Action) -> Result<()> {
    let client = &ctx.wrap.client;
    let signer = &ctx.wrap.signer;
    let refs = ctx.refs();
    match action {
        Action::CrankRedeem { bucket, call_type } => {
            vault_tx::crank_redeem(client, signer, &refs, *bucket, call_type, ctx.gas_budget)
                .await?;
        }
        Action::SettleRfq { rfq, bucket, call_type } => {
            vault_tx::settle_rfq(
                client,
                signer,
                &refs,
                *rfq,
                *bucket,
                call_type,
                ctx.protocol_config_id,
                ctx.treasury_id,
                ctx.gas_budget,
            )
            .await?;
        }
        Action::SettleRfqExpired { rfq, bucket, call_type } => {
            vault_tx::settle_rfq_expired(
                client, signer, &refs, *rfq, *bucket, call_type, ctx.gas_budget,
            )
            .await?;
        }
        Action::OpenSwapRfq { amount_s } => {
            let mut pt = ctx.pt_with_price_update().await?;
            vault_tx::build_open_swap_rfq(client, &mut pt, &refs, *amount_s, &ctx.prices()).await?;
            submit_ptb(client, signer, pt, ctx.gas_budget, "vault::open_swap_rfq").await?;
        }
        Action::SettleSwapRfq { swap_rfq } => {
            let mut pt = ctx.pt_with_price_update().await?;
            vault_tx::build_settle_swap_rfq(client, &mut pt, &refs, *swap_rfq, &ctx.prices())
                .await?;
            submit_ptb(client, signer, pt, ctx.gas_budget, "vault::settle_swap_rfq").await?;
        }
        Action::FinalizeRound => {
            let mut pt = ctx.pt_with_price_update().await?;
            vault_tx::build_finalize_round(client, &mut pt, &refs, ctx.treasury_id, &ctx.prices())
                .await?;
            submit_ptb(client, signer, pt, ctx.gas_budget, "vault::finalize_round").await?;
        }
        Action::OpenRfq { bucket, call_type, slice_amount } => {
            let mut pt = ctx.pt_with_price_update().await?;
            vault_tx::build_open_rfq(
                client,
                &mut pt,
                &refs,
                *bucket,
                call_type,
                *slice_amount,
                &ctx.prices(),
            )
            .await?;
            submit_ptb(client, signer, pt, ctx.gas_budget, "vault::open_rfq").await?;
        }
        Action::SelectBucketNeeded | Action::Idle => {
            // Resolved by the tick loop before reaching here.
            return Err(anyhow!("{action:?} is not directly submittable"));
        }
    }
    info!(vault = %ctx.vault_id, ?action, "action submitted");
    Ok(())
}

/// Submit `select_bucket` (the tick loop resolves the pick first).
pub async fn execute_select_bucket(
    ctx: &SubmitCtx<'_>,
    bucket: ObjectID,
    call_type: &str,
) -> Result<()> {
    let mut pt = ctx.pt_with_price_update().await?;
    vault_tx::build_select_bucket(
        &ctx.wrap.client,
        &mut pt,
        &ctx.refs(),
        bucket,
        call_type,
        &ctx.prices(),
    )
    .await?;
    submit_ptb(&ctx.wrap.client, &ctx.wrap.signer, pt, ctx.gas_budget, "vault::select_bucket")
        .await?;
    Ok(())
}

// ── error triage (README §10) ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Another keeper won the race or state moved under us — drop it,
    /// the next tick replans from fresh state.
    Benign,
    /// Transient (RPC/Hermes/thin book/stale oracle) — same: next tick.
    Retry,
    /// Config or deployment bug — alert and halt this vault.
    Fatal,
}

/// Move abort codes that mean "the state changed between read and
/// submit" (`contracts/sources/errors.move`).
const BENIGN_ABORTS: &[u64] = &[
    21, // zero_amount: proceeds swapped by a racer
    29, // rfq_closed
    30, // rfq_not_closed (deadline got sniped out)
    35, // vault_wrong_phase
    36, // vault_bucket_not_selected
    37, // vault_bucket_already_selected
    38, // vault_selling_closed
    39, // vault_positions_pending
    40, // vault_rfqs_open
    41, // vault_round_not_finalized
    43, // vault_strike_out_of_band (spot moved since the pick)
    44, // vault_expiry_out_of_band
    45, // vault_slice_too_large (deployable moved)
    46, // vault_too_many_rfqs
];

/// Aborts that retry cleanly next tick with fresh inputs.
const RETRY_ABORTS: &[u64] = &[
    50, // oracle_price_stale: refetch hermes
    51, // oracle_confidence
    52, // oracle_price_invalid
    53, // vault_proceeds_unswapped: thin book, partial later
];

/// Config/deployment bugs: retrying cannot help.
const FATAL_ABORTS: &[u64] = &[
    32, // rfq_bucket_mismatch
    42, // vault_receipt_round_mismatch
    48, // vault_wrong_origin
    49, // oracle_feed_mismatch
    54, // vault_config_invalid
];

/// Pull the abort code out of a revert message like
/// `… MoveAbort(MoveLocation { … }, 43) in command 2`.
pub fn extract_abort_code(msg: &str) -> Option<u64> {
    let after = &msg[msg.find("MoveAbort(")? + "MoveAbort(".len()..];
    // The location debug-print nests braces (`ModuleId { … }`), so scan
    // every `}, ` for the one followed by `<digits>)` — the abort code.
    for (i, _) in after.match_indices("}, ") {
        let rest = &after[i + 3..];
        if let Some(end) = rest.find(')') {
            if let Ok(code) = rest[..end].trim().parse() {
                return Some(code);
            }
        }
    }
    None
}

pub fn classify(err: &anyhow::Error) -> ErrorClass {
    let msg = format!("{err:#}");
    if let Some(code) = extract_abort_code(&msg) {
        if BENIGN_ABORTS.contains(&code) {
            return ErrorClass::Benign;
        }
        if RETRY_ABORTS.contains(&code) {
            return ErrorClass::Retry;
        }
        if FATAL_ABORTS.contains(&code) {
            return ErrorClass::Fatal;
        }
        // Unknown abort: treat as fatal-ish so it gets eyes, but only
        // after the planner had a chance to re-derive — Retry is the
        // safer default for a stateless keeper.
        return ErrorClass::Retry;
    }
    let lower = msg.to_lowercase();
    let fatal_patterns =
        ["parsing underlying type", "parsing settlement type", "parsing call type",
         "parsing share type", "parsing deep type", "not found on chain", "is not shared"];
    if fatal_patterns.iter().any(|p| lower.contains(p)) {
        return ErrorClass::Fatal;
    }
    ErrorClass::Retry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revert(code: u64) -> anyhow::Error {
        anyhow!(
            "vault::select_bucket reverted: MoveAbort(MoveLocation {{ module: ModuleId {{ \
             address: abc, name: Identifier(\"vault\") }}, function: 14, instruction: 35, \
             function_name: Some(\"select_bucket\") }}, {code}) in command 7"
        )
    }

    #[test]
    fn extracts_abort_codes() {
        assert_eq!(extract_abort_code(&format!("{:#}", revert(43))), Some(43));
        assert_eq!(extract_abort_code("no abort here"), None);
    }

    #[test]
    fn classifies_race_aborts_benign() {
        for code in [35, 37, 40, 43] {
            assert_eq!(classify(&revert(code)), ErrorClass::Benign, "code {code}");
        }
    }

    #[test]
    fn classifies_transient_aborts_retry() {
        for code in [50, 53] {
            assert_eq!(classify(&revert(code)), ErrorClass::Retry, "code {code}");
        }
    }

    #[test]
    fn classifies_config_aborts_fatal() {
        for code in [49, 54] {
            assert_eq!(classify(&revert(code)), ErrorClass::Fatal, "code {code}");
        }
    }

    #[test]
    fn classifies_non_abort_errors() {
        assert_eq!(
            classify(&anyhow!("parsing underlying type 0xgarbage: bad")),
            ErrorClass::Fatal
        );
        assert_eq!(classify(&anyhow!("connection timed out")), ErrorClass::Retry);
    }
}
