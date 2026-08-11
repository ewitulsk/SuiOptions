//! Matched-mode settlement submitter (spec §5.6) over the workspace's gRPC
//! stack: builds `exchange::settlement::match_orders` PTBs client-side with
//! `sui-tx`, submits via `submit_ptb_rebuilding` (shared-object and gas
//! references are re-read per attempt), and decodes Move aborts into
//! targeted reactions — prune the over-committed maker, drop the dead
//! order, restore the counterparty.

use std::str::FromStr;

use anyhow::{Context, Result};
use exchange_book::MatchIntent;
use exchange_types::Digest;
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_tx::chain::ChainClient;
use sui_tx::sui_client::Signer;
use sui_tx::tx::{clock_arg, submit_ptb_rebuilding, tx_digest};
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

use crate::db::{Db, StoredOrder};

/// Shared trading-vault objects the direct-escrow entries need (SO-372),
/// resolved once at boot. `None` ⇒ direct escrow disabled.
#[derive(Clone, Debug)]
pub struct DirectEscrow {
    pub adapter_package: ObjectID,
    pub integration_registry: ObjectID,
}

/// A DIRECT maker's vault binding, from the manager-mode map: its manager
/// is identity-only, so fills/matches must go through the exchange-adapter
/// entries with the vault + custody in the PTB.
#[derive(Clone, Debug)]
pub struct VaultMaker {
    pub vault_id: ObjectID,
    pub custody_id: ObjectID,
}

/// Look up a maker manager in the manager-mode map. `Some` only for DIRECT
/// custodies — funded custody managers settle through the classic entries
/// like plain wallet BMs. Lookup failures degrade to the classic path.
pub async fn vault_maker_of(
    db: &Db,
    manager: &exchange_types::SuiAddress,
) -> Option<VaultMaker> {
    match db.vault_manager(manager).await {
        Ok(Some(row)) if row.direct => {
            let vault_id = ObjectID::from_hex_literal(&row.vault_id).ok()?;
            let custody_id = ObjectID::from_hex_literal(&row.custody_id).ok()?;
            Some(VaultMaker { vault_id, custody_id })
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(manager = %manager, error = %e, "vault-manager lookup failed; assuming classic BM");
            None
        }
    }
}

/// Everything needed to settle one match on-chain. The service layer
/// resolves the intent's digests into stored signed orders; `ask` is always
/// the Base-selling order (`order_a` on-chain). The `*_vault` bindings are
/// `Some` for direct-escrow makers (SO-372).
#[derive(Clone, Debug)]
pub struct MatchJob {
    pub intent: MatchIntent,
    pub ask: StoredOrder,
    pub bid: StoredOrder,
    /// Canonical type strings of the market pair.
    pub base_type: String,
    pub quote_type: String,
    pub ask_vault: Option<VaultMaker>,
    pub bid_vault: Option<VaultMaker>,
}

/// Decoded settlement outcome. The abort mapping mirrors the Move modules'
/// error constants — keep in sync with settlement.move / balance_manager.move
/// / registry.move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettleOutcome {
    Confirmed { tx_digest: String },
    /// Order-level death: drop this digest, restore + re-match the other.
    OrderDead { digest: Digest, reason: DeadReason },
    /// Maker escrow can't cover: prune/downsize that maker's orders.
    InsufficientEscrow,
    /// A DIRECT maker's vault free balance can't cover its leg (SO-372).
    /// Side-tagged by the abort: `ask_side` ⇒ the base-selling maker.
    VaultEscrowInsufficient { ask_side: bool },
    /// A direct maker's quoting is off (vault not open / adapter delisted /
    /// curator opt-out): prune all of that manager's orders.
    VaultQuotingDisabled,
    /// Shared-object congestion cancelled execution (nothing recorded);
    /// restore and re-match — retryable, never prune-worthy.
    Congested,
    /// Stale fill state (someone raced us): restore both, refresh, re-match.
    Stale,
    /// Transport/gas failure.
    Failed { error: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadReason {
    Expired,
    Cancelled,
    SaltVoided,
    BadSignature,
}

// settlement.move abort codes
const E_EXPIRED: u64 = 3;
const E_SALT_VOIDED: u64 = 7;
const E_CANCELLED: u64 = 8;
const E_ALREADY_FILLED: u64 = 9;
const E_BAD_SIGNATURE: u64 = 10;
const E_NOT_CROSSING: u64 = 15;
const E_LIMIT_VIOLATED: u64 = 16;
// registry.move
const E_OVERFILL: u64 = 3;
// balance_manager.move
const E_INSUFFICIENT_ESCROW: u64 = 2;
// exchange_adapter.move (SO-372 direct escrow)
const EA_INSUFFICIENT_ESCROW_A: u64 = 9;
const EA_INSUFFICIENT_ESCROW_B: u64 = 10;
// trading_vault vault.move quote-session gates (module name "vault")
const TV_VAULT_NOT_OPEN: u64 = 72;
const TV_ADAPTER_NOT_ALLOWED: u64 = 75;
const TV_QUOTE_ADAPTER_NOT_ENABLED: u64 = 113;

/// A decoded Move abort: (module name, abort code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveAbort {
    pub module: String,
    pub code: u64,
}

/// Best-effort parse of a MoveAbort out of an error string (the Debug
/// rendering of `ExecutionStatus` that `sui_tx::tx::assert_success` bails
/// with): grabs the module `Identifier("…")` and the `}, <code>)` that
/// closes the `MoveLocation`.
pub fn parse_move_abort(error: &str) -> Option<MoveAbort> {
    let idx = error.find("MoveAbort(")?;
    let rest = &error[idx..];
    let name_key = "Identifier(\"";
    let n = rest.find(name_key)? + name_key.len();
    let name_end = rest[n..].find('"')? + n;
    let module = rest[n..name_end].to_string();
    // the abort code is the first `}, <digits>)` after the module name
    let mut search = &rest[name_end..];
    loop {
        let brace = search.find("}, ")?;
        let after = &search[brace + 3..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with(')') {
            return Some(MoveAbort { module, code: digits.parse().ok()? });
        }
        search = &search[brace + 3..];
    }
}

pub struct Submitter {
    chain: ChainClient,
    signer: Signer,
    package: ObjectID,
    /// Direct-escrow ids (SO-372); `None` disables the adapter targets.
    direct: Option<DirectEscrow>,
    gas_budget: u64,
}

impl Submitter {
    pub fn new(
        chain: ChainClient,
        signer: Signer,
        package: ObjectID,
        direct: Option<DirectEscrow>,
        gas_budget: u64,
    ) -> Self {
        Submitter { chain, signer, package, direct, gas_budget }
    }

    pub fn relayer_address(&self) -> sui_types::base_types::SuiAddress {
        self.signer.address
    }

    /// Settle one match intent: build, sign, submit, decode. Stale gas /
    /// object references are retried inside `submit_ptb_rebuilding` (those
    /// are rejections, nothing executed); anything else is decoded once —
    /// a blind resubmit could double-settle.
    pub async fn submit_match(&self, job: &MatchJob) -> SettleOutcome {
        match self.try_submit(job).await {
            Ok(resp) => SettleOutcome::Confirmed { tx_digest: tx_digest(&resp).to_string() },
            Err(e) => {
                let msg = format!("{e:#}");
                // Vault-involved fills touch more shared objects (vault(s) +
                // registry mutably) and see more congestion; a cancelled
                // execution records nothing, so re-matching is safe.
                if is_congestion_cancellation(&msg) {
                    return SettleOutcome::Congested;
                }
                match parse_move_abort(&msg) {
                    Some(abort) => decode_abort(&abort, job, &msg),
                    None => SettleOutcome::Failed { error: msg },
                }
            }
        }
    }

    /// The Move target for a job, by which sides are direct-escrow makers
    /// (SO-372): classic `settlement::match_orders` when neither is.
    fn label(job: &MatchJob) -> &'static str {
        match (&job.ask_vault, &job.bid_vault) {
            (None, None) => "exchange settlement::match_orders",
            (Some(_), None) => "exchange_adapter::match_vault_vs_bm",
            (None, Some(_)) => "exchange_adapter::match_bm_vs_vault",
            (Some(_), Some(_)) => "exchange_adapter::match_vault_vs_vault",
        }
    }

    async fn try_submit(&self, job: &MatchJob) -> Result<sui_tx::chain::ExecutedTransaction> {
        let type_args = vec![
            TypeTag::from_str(&job.base_type)
                .with_context(|| format!("parsing base type {}", job.base_type))?,
            TypeTag::from_str(&job.quote_type)
                .with_context(|| format!("parsing quote type {}", job.quote_type))?,
        ];
        let registry = ObjectID::new(job.ask.signed.registry_id.0);
        let bm_a = ObjectID::new(job.ask.signed.order.maker_manager_id.0);
        let bm_b = ObjectID::new(job.bid.signed.order.maker_manager_id.0);
        if (job.ask_vault.is_some() || job.bid_vault.is_some()) && self.direct.is_none() {
            anyhow::bail!("direct-escrow maker but no exchange_adapter deployment configured");
        }

        submit_ptb_rebuilding(
            &self.chain,
            &self.signer,
            self.gas_budget,
            Self::label(job),
            || {
                let type_args = type_args.clone();
                async move {
                    let mut pt = ProgrammableTransactionBuilder::new();
                    // Pure inputs are shared by every target shape.
                    let a_bytes = pt.pure(job.ask.signed.order.to_bcs())?;
                    let a_sig = pt.pure(job.ask.signed.prefixed_signature())?;
                    let a_pk = pt.pure(job.ask.signed.public_key.clone())?;
                    let b_bytes = pt.pure(job.bid.signed.order.to_bcs())?;
                    let b_sig = pt.pure(job.bid.signed.prefixed_signature())?;
                    let b_pk = pt.pure(job.bid.signed.public_key.clone())?;
                    let fill = pt.pure(job.intent.fill_base_amount)?;
                    let orders = [a_bytes, a_sig, a_pk, b_bytes, b_sig, b_pk];
                    let reg_arg = pt.obj(self.chain.shared_object_arg(registry, true).await?)?;

                    match (&job.ask_vault, &job.bid_vault) {
                        // Classic path: both makers funded — unchanged.
                        (None, None) => {
                            let bm_a_arg =
                                pt.obj(self.chain.shared_object_arg(bm_a, true).await?)?;
                            let bm_b_arg =
                                pt.obj(self.chain.shared_object_arg(bm_b, true).await?)?;
                            let clock = clock_arg(&mut pt)?;
                            pt.programmable_move_call(
                                self.package,
                                Identifier::new("settlement")?,
                                Identifier::new("match_orders")?,
                                type_args,
                                vec![
                                    reg_arg, bm_a_arg, bm_b_arg, orders[0], orders[1], orders[2],
                                    orders[3], orders[4], orders[5], fill, clock,
                                ],
                            );
                        }
                        // Vault sells Base against a funded maker.
                        (Some(av), None) => {
                            let d = self.direct.as_ref().expect("checked above");
                            let vault =
                                pt.obj(self.chain.shared_object_arg(av.vault_id, true).await?)?;
                            let vreg = pt.obj(
                                self.chain
                                    .shared_object_arg(d.integration_registry, false)
                                    .await?,
                            )?;
                            let bm_a_arg =
                                pt.obj(self.chain.shared_object_arg(bm_a, false).await?)?;
                            let custody = pt.pure(av.custody_id)?;
                            let bm_b_arg =
                                pt.obj(self.chain.shared_object_arg(bm_b, true).await?)?;
                            let clock = clock_arg(&mut pt)?;
                            pt.programmable_move_call(
                                d.adapter_package,
                                Identifier::new("exchange_adapter")?,
                                Identifier::new("match_vault_vs_bm")?,
                                type_args,
                                vec![
                                    vault, vreg, reg_arg, bm_a_arg, custody, bm_b_arg, orders[0],
                                    orders[1], orders[2], orders[3], orders[4], orders[5], fill,
                                    clock,
                                ],
                            );
                        }
                        // Funded maker sells Base against the vault's Quote.
                        (None, Some(bv)) => {
                            let d = self.direct.as_ref().expect("checked above");
                            let vault =
                                pt.obj(self.chain.shared_object_arg(bv.vault_id, true).await?)?;
                            let vreg = pt.obj(
                                self.chain
                                    .shared_object_arg(d.integration_registry, false)
                                    .await?,
                            )?;
                            let bm_a_arg =
                                pt.obj(self.chain.shared_object_arg(bm_a, true).await?)?;
                            let bm_b_arg =
                                pt.obj(self.chain.shared_object_arg(bm_b, false).await?)?;
                            let custody = pt.pure(bv.custody_id)?;
                            let clock = clock_arg(&mut pt)?;
                            pt.programmable_move_call(
                                d.adapter_package,
                                Identifier::new("exchange_adapter")?,
                                Identifier::new("match_bm_vs_vault")?,
                                type_args,
                                vec![
                                    vault, vreg, reg_arg, bm_a_arg, bm_b_arg, custody, orders[0],
                                    orders[1], orders[2], orders[3], orders[4], orders[5], fill,
                                    clock,
                                ],
                            );
                        }
                        // Both sides vaults.
                        (Some(av), Some(bv)) => {
                            let d = self.direct.as_ref().expect("checked above");
                            let vault_a =
                                pt.obj(self.chain.shared_object_arg(av.vault_id, true).await?)?;
                            let custody_a = pt.pure(av.custody_id)?;
                            let bm_a_arg =
                                pt.obj(self.chain.shared_object_arg(bm_a, false).await?)?;
                            let vault_b =
                                pt.obj(self.chain.shared_object_arg(bv.vault_id, true).await?)?;
                            let custody_b = pt.pure(bv.custody_id)?;
                            let bm_b_arg =
                                pt.obj(self.chain.shared_object_arg(bm_b, false).await?)?;
                            let vreg = pt.obj(
                                self.chain
                                    .shared_object_arg(d.integration_registry, false)
                                    .await?,
                            )?;
                            let clock = clock_arg(&mut pt)?;
                            pt.programmable_move_call(
                                d.adapter_package,
                                Identifier::new("exchange_adapter")?,
                                Identifier::new("match_vault_vs_vault")?,
                                type_args,
                                vec![
                                    vault_a, custody_a, bm_a_arg, vault_b, custody_b, bm_b_arg,
                                    vreg, reg_arg, orders[0], orders[1], orders[2], orders[3],
                                    orders[4], orders[5], fill, clock,
                                ],
                            );
                        }
                    }
                    Ok(pt.finish())
                }
            },
        )
        .await
    }
}

/// Sui cancels (never partially applies) transactions that lose shared-object
/// congestion control; the fill is not recorded, so re-matching is safe.
/// Matched on the effects-status Debug string, like `parse_move_abort`.
pub fn is_congestion_cancellation(error: &str) -> bool {
    error.contains("SharedObjectCongestion") || error.contains("CongestedObjects")
}

/// Map an on-chain abort to the submitter's reaction (spec §5.6).
///
/// For per-order aborts (expiry, cancel, salt) the abort itself doesn't say
/// which order failed; attribution uses the orders' own fields where
/// possible, defaulting to the ask (validated first on-chain) — the caller
/// re-validates both against local state anyway.
pub fn decode_abort(abort: &MoveAbort, job: &MatchJob, raw: &str) -> SettleOutcome {
    match (abort.module.as_str(), abort.code) {
        ("balance_manager", E_INSUFFICIENT_ESCROW) => SettleOutcome::InsufficientEscrow,
        // SO-372: the adapter side-tags starved vault escrow so exactly the
        // starved maker's orders get pruned (A = base seller = the ask).
        ("exchange_adapter", EA_INSUFFICIENT_ESCROW_A) => {
            SettleOutcome::VaultEscrowInsufficient { ask_side: true }
        }
        ("exchange_adapter", EA_INSUFFICIENT_ESCROW_B) => {
            SettleOutcome::VaultEscrowInsufficient { ask_side: false }
        }
        // trading_vault quote-session gates: the maker's direct quoting is
        // off (vault not open / adapter delisted / curator opt-out).
        ("vault", TV_VAULT_NOT_OPEN)
        | ("vault", TV_ADAPTER_NOT_ALLOWED)
        | ("vault", TV_QUOTE_ADAPTER_NOT_ENABLED) => SettleOutcome::VaultQuotingDisabled,
        ("registry", E_OVERFILL) => SettleOutcome::Stale,
        ("settlement", E_ALREADY_FILLED)
        | ("settlement", E_NOT_CROSSING)
        | ("settlement", E_LIMIT_VIOLATED) => SettleOutcome::Stale,
        ("settlement", E_EXPIRED) => {
            let dead = if job.ask.signed.order.expiry_ms <= job.bid.signed.order.expiry_ms {
                job.ask.digest
            } else {
                job.bid.digest
            };
            SettleOutcome::OrderDead { digest: dead, reason: DeadReason::Expired }
        }
        ("settlement", E_CANCELLED) => {
            SettleOutcome::OrderDead { digest: job.ask.digest, reason: DeadReason::Cancelled }
        }
        ("settlement", E_SALT_VOIDED) => {
            SettleOutcome::OrderDead { digest: job.ask.digest, reason: DeadReason::SaltVoided }
        }
        ("settlement", E_BAD_SIGNATURE) => {
            SettleOutcome::OrderDead { digest: job.ask.digest, reason: DeadReason::BadSignature }
        }
        _ => SettleOutcome::Failed { error: raw.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange_types::order::{Order, SignatureScheme, SignedOrder};
    use exchange_types::{Side, SuiAddress};

    fn stored(expiry: u64) -> StoredOrder {
        let order = Order {
            maker_token: "B".into(),
            taker_token: "Q".into(),
            maker_amount: 10,
            taker_amount: 20,
            max_fee_bps: 10,
            maker: SuiAddress::ZERO,
            maker_manager_id: SuiAddress::ZERO,
            taker: SuiAddress::ZERO,
            sender: SuiAddress::ZERO,
            expiry_ms: expiry,
            salt: 1,
        };
        StoredOrder {
            digest: Digest([expiry as u8; 32]),
            signed: SignedOrder {
                order,
                registry_id: SuiAddress::ZERO,
                scheme: SignatureScheme::Ed25519,
                signature: vec![0; 64],
                public_key: vec![0; 32],
            },
            side: Side::Ask,
            price_ticks: 1,
            filled_taker: 0,
            status: "OPEN".into(),
        }
    }

    fn job() -> MatchJob {
        MatchJob {
            intent: MatchIntent {
                market: SuiAddress::ZERO,
                ask_digest: Digest([1; 32]),
                bid_digest: Digest([2; 32]),
                fill_base_amount: 10,
                exec_price_ticks: 1,
            },
            ask: stored(1),
            bid: stored(2),
            base_type: "B".into(),
            quote_type: "Q".into(),
            ask_vault: None,
            bid_vault: None,
        }
    }

    /// The exact shape `assert_success` bails with: the Debug rendering of
    /// `ExecutionStatus::Failure`.
    fn abort_msg(module: &str, code: u64) -> String {
        format!(
            "exchange settlement::match_orders reverted: Failure {{ error: MoveAbort(MoveLocation {{ module: ModuleId {{ address: abc123, name: Identifier(\"{module}\") }}, function: 12, instruction: 33, function_name: Some(\"match_orders\") }}, {code}), command: Some(0) }}"
        )
    }

    #[test]
    fn abort_parsing_handles_debug_format() {
        assert_eq!(
            parse_move_abort(&abort_msg("settlement", 9)),
            Some(MoveAbort { module: "settlement".into(), code: 9 })
        );
        assert_eq!(parse_move_abort("InsufficientGas"), None);
    }

    #[test]
    fn decode_targeted_reactions() {
        let j = job();
        let decode = |m: &str, c: u64| {
            let msg = abort_msg(m, c);
            decode_abort(&parse_move_abort(&msg).unwrap(), &j, &msg)
        };
        assert_eq!(decode("balance_manager", 2), SettleOutcome::InsufficientEscrow);
        assert_eq!(decode("settlement", 9), SettleOutcome::Stale);
        assert_eq!(decode("registry", 3), SettleOutcome::Stale);
        // expiry attributes the earlier-expiring order (the ask, expiry 1)
        assert_eq!(
            decode("settlement", 3),
            SettleOutcome::OrderDead { digest: Digest([1; 32]), reason: DeadReason::Expired }
        );
        assert!(matches!(
            decode("settlement", 99),
            SettleOutcome::Failed { .. }
        ));
    }

    /// SO-372: adapter escrow aborts are side-tagged; trading-vault quote
    /// gates prune the maker; congestion cancellations are retryable.
    #[test]
    fn decode_direct_escrow_reactions() {
        let j = job();
        let decode = |m: &str, c: u64| {
            let msg = abort_msg(m, c);
            decode_abort(&parse_move_abort(&msg).unwrap(), &j, &msg)
        };
        assert_eq!(
            decode("exchange_adapter", 9),
            SettleOutcome::VaultEscrowInsufficient { ask_side: true }
        );
        assert_eq!(
            decode("exchange_adapter", 10),
            SettleOutcome::VaultEscrowInsufficient { ask_side: false }
        );
        assert_eq!(decode("vault", 113), SettleOutcome::VaultQuotingDisabled);
        assert_eq!(decode("vault", 72), SettleOutcome::VaultQuotingDisabled);
        assert_eq!(decode("vault", 75), SettleOutcome::VaultQuotingDisabled);
        // Unknown adapter/vault aborts stay loud.
        assert!(matches!(decode("exchange_adapter", 12), SettleOutcome::Failed { .. }));

        assert!(is_congestion_cancellation(
            "exchange_adapter::match_vault_vs_vault reverted: Failure { error: \
             ExecutionCancelledDueToSharedObjectCongestion { congested_objects: \
             CongestedObjects([0xfe]) }, command: None }"
        ));
        assert!(!is_congestion_cancellation("InsufficientGas"));
    }
}
