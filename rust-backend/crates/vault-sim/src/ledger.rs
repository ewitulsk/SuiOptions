//! Integer share/fee accounting — the reference implementation of the
//! vault's round accounting (doc 03 §7.4), in chain units with the exact
//! rounding the Move contract must reproduce: u128 PPS at `PPS_SCALE`,
//! floor on share mint and withdraw, the profitable-round fee gate, queue
//! ordering (withdrawals then deposits at `pps[r]`), and the receipt-round
//! convention (a round-`r` receipt converts at `pps[r − 1]`).
//!
//! Two behaviors doc 03 leaves implicit are pinned down here and must be
//! copied by `vault.move`:
//!
//! 1. **Unclaimed deposit shares count.** The §7.4 `shares` term must
//!    include shares whose `DepositReceipt` was never claimed, or finalize
//!    would re-credit those depositors' funds to everyone else. The ledger
//!    therefore "mints" the aggregate `floor(pending × PPS_SCALE / pps[r])`
//!    into supply at finalize; `claim_shares` only allocates out of that
//!    pool (per-receipt floor dust, ≤ 1 unit per receipt, stays in the
//!    pool).
//! 2. **Fee-cap split.** When `mgmt + perf` exceeds round profit, the
//!    management fee is charged first and the performance fee absorbs the
//!    cap: `mgmt' = min(mgmt, profit)`, `perf' = min(perf, profit − mgmt')`.
//!    The total equals `min(mgmt + perf, profit)` either way; the split is
//!    what `VaultFeesCharged` reports.
//!
//! One rounding note: with capped fees, the locked pps can land 1 unit of
//! `PPS_SCALE` (1e-12) below `pps_prev` — the unavoidable double-floor of
//! `floor(floor(shares × pps_prev / S) × S / shares)`. "Fees never push pps
//! below pps_prev" holds to that 1-ulp.

use thiserror::Error;

use crate::types::{Pps, SettleAmt, ShareAmt, UnderlyingAmt, PPS_SCALE};

/// 365-day year, matching the worked example in doc 03 §8.
pub const YEAR_MS: u64 = 31_536_000_000;

#[derive(Clone, Copy, Debug)]
pub struct LedgerConfig {
    /// Annualized management fee in bps of AUM (e.g. 200 = 2%/yr).
    pub mgmt_fee_bps_annual: u64,
    /// Performance fee in bps of the round's premium (e.g. 1000 = 10%).
    pub perf_fee_bps: u64,
    /// Round length used to prorate the management fee.
    pub round_ms: u64,
}

/// Claim ticket for a queued deposit. `round` is the round the deposit
/// participates from; it converts at `pps[round − 1]` (doc 03 §7.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositReceipt {
    pub round: u64,
    pub amount: UnderlyingAmt,
}

/// Claim ticket for a two-step withdrawal. Pays `shares × pps[round] /
/// PPS_SCALE` once round `round` is finalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithdrawReceipt {
    pub round: u64,
    pub shares: ShareAmt,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("round {0} has not been finalized (no pps locked)")]
    RoundNotPriced(u64),
    #[error("deposit's round already started; use the withdrawal queue")]
    DepositRoundStarted,
}

/// Everything `finalize_round` decided, mirroring `VaultRoundFinalized` +
/// `VaultFeesCharged` event contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizeOutcome {
    /// The round that was just finalized (pps index).
    pub round: u64,
    pub pps: Pps,
    /// Deployable AUM at finalize, pre-fee, pre-queue.
    pub aum: UnderlyingAmt,
    /// Live shares the round's P&L accrued to (supply + queued).
    pub shares: ShareAmt,
    pub mgmt_fee: UnderlyingAmt,
    pub perf_fee: UnderlyingAmt,
    /// Underlying moved to the withdrawal pool for queued shares.
    pub withdrawals_owed: UnderlyingAmt,
    /// Queued shares burned at `pps`.
    pub shares_burned: ShareAmt,
    /// Pending deposits that joined deployable.
    pub deposits_processed: UnderlyingAmt,
    /// Shares minted (in aggregate) for those deposits.
    pub shares_minted: ShareAmt,
}

impl FinalizeOutcome {
    pub fn fees(&self) -> UnderlyingAmt {
        self.mgmt_fee + self.perf_fee
    }
}

/// The vault's share/balance state machine, one instance per simulated
/// vault. Round `0` is genesis: the first `finalize_round` locks
/// `pps[0] = PPS_SCALE` (no live shares) and activates round 1 with the
/// initial deposits.
#[derive(Clone, Debug)]
pub struct Ledger {
    cfg: LedgerConfig,
    round: u64,
    /// `pps[r]`, set exactly once when round `r` finalizes.
    pps: Vec<Pps>,
    /// Outstanding shares, including aggregate-minted unclaimed deposit
    /// shares, excluding escrowed (queued) shares.
    share_supply: ShareAmt,
    /// Subset of `share_supply` minted at finalize but not yet allocated
    /// to a `DepositReceipt` via `claim_shares`.
    claimable_shares: ShareAmt,
    /// Shares escrowed by `initiate_withdraw`, burned at next finalize.
    queued_withdraw_shares: ShareAmt,
    /// Assets working in the current round.
    deployable: UnderlyingAmt,
    /// Queued deposits; join deployable at the next finalize.
    pending_deposits: UnderlyingAmt,
    /// Fully reserved for completed `WithdrawReceipt`s.
    withdrawal_pool: UnderlyingAmt,
    /// Cumulative fees transferred out to the protocol treasury.
    fees_paid: UnderlyingAmt,
}

impl Ledger {
    pub fn new(cfg: LedgerConfig) -> Self {
        Self {
            cfg,
            round: 0,
            pps: Vec::new(),
            share_supply: ShareAmt::ZERO,
            claimable_shares: ShareAmt::ZERO,
            queued_withdraw_shares: ShareAmt::ZERO,
            deployable: UnderlyingAmt::ZERO,
            pending_deposits: UnderlyingAmt::ZERO,
            withdrawal_pool: UnderlyingAmt::ZERO,
            fees_paid: UnderlyingAmt::ZERO,
        }
    }

    /// Test-only state injection — the sim twin of doc 03 §12's "manual
    /// pps setter under #[test_only]": share math can be asserted from an
    /// arbitrary mid-life state without replaying the rounds that built it.
    #[cfg(test)]
    pub(crate) fn for_test_with_state(
        cfg: LedgerConfig,
        pps_history: Vec<Pps>,
        share_supply: ShareAmt,
        deployable: UnderlyingAmt,
    ) -> Self {
        let round = pps_history.len() as u64;
        Self {
            cfg,
            round,
            pps: pps_history,
            share_supply,
            claimable_shares: ShareAmt::ZERO,
            queued_withdraw_shares: ShareAmt::ZERO,
            deployable,
            pending_deposits: UnderlyingAmt::ZERO,
            withdrawal_pool: UnderlyingAmt::ZERO,
            fees_paid: UnderlyingAmt::ZERO,
        }
    }

    // ---- user functions (doc 03 §5) ----

    /// Queue a deposit for the next round (doc 03 §5.1).
    pub fn deposit(&mut self, amount: UnderlyingAmt) -> DepositReceipt {
        assert!(amount.0 > 0, "zero amount");
        self.pending_deposits += amount;
        DepositReceipt { round: self.round + 1, amount }
    }

    /// Convert a deposit receipt to shares at `pps[round − 1]` (doc 03
    /// §5.2). Available as soon as that pps entry exists.
    pub fn claim_shares(&mut self, receipt: DepositReceipt) -> Result<ShareAmt, LedgerError> {
        let pps = self.pps_at(receipt.round - 1)?;
        let shares = pps.underlying_to_shares(receipt.amount);
        // The aggregate mint at finalize is ≥ the sum of per-receipt
        // floors, so allocation can never run dry.
        self.claimable_shares -= shares;
        Ok(shares)
    }

    /// Escrow shares for a two-step withdrawal (doc 03 §5.3). The shares
    /// stay exposed to the current round's P&L.
    pub fn initiate_withdraw(&mut self, shares: ShareAmt) -> WithdrawReceipt {
        assert!(shares.0 > 0, "zero amount");
        assert!(
            shares + self.queued_withdraw_shares
                <= self.share_supply + self.queued_withdraw_shares - self.claimable_shares,
            "withdrawing more shares than allocated"
        );
        self.share_supply -= shares;
        self.queued_withdraw_shares += shares;
        WithdrawReceipt { round: self.round, shares }
    }

    /// Pay out a finalized withdrawal at its locked pps (doc 03 §5.4).
    pub fn complete_withdraw(
        &mut self,
        receipt: WithdrawReceipt,
    ) -> Result<UnderlyingAmt, LedgerError> {
        let pps = self.pps_at(receipt.round)?;
        let owed = pps.shares_to_underlying(receipt.shares);
        self.withdrawal_pool -= owed;
        Ok(owed)
    }

    /// Cancel a deposit whose round hasn't started (doc 03 §5.5).
    pub fn instant_withdraw_pending(
        &mut self,
        receipt: DepositReceipt,
    ) -> Result<UnderlyingAmt, LedgerError> {
        if receipt.round <= self.round {
            return Err(LedgerError::DepositRoundStarted);
        }
        self.pending_deposits -= receipt.amount;
        Ok(receipt.amount)
    }

    // ---- finalize (doc 03 §7.4) ----

    /// Finalize the current round. `aum` is the deployable value after all
    /// redeems and swaps (it excludes `pending_deposits` and
    /// `withdrawal_pool`, which this ledger holds separately);
    /// `premium_in_underlying` is the round's net premium converted to
    /// underlying at the swap execution (or Pyth spot under the
    /// hold-premium policy). The engine owns both conversions.
    pub fn finalize_round(
        &mut self,
        aum: UnderlyingAmt,
        premium_in_underlying: UnderlyingAmt,
    ) -> FinalizeOutcome {
        let round = self.round;
        let shares = self.share_supply + self.queued_withdraw_shares;
        let pps_prev = if round == 0 { Pps::ONE } else { self.pps[round as usize - 1] };

        let pps_gross = if shares.0 == 0 {
            pps_prev
        } else {
            Pps((aum.0 as u128 * PPS_SCALE) / shares.0 as u128)
        };

        // Fees only on profitable rounds, capped so they can never push
        // pps below pps_prev (to the 1-ulp double-floor note above).
        let (mgmt_fee, perf_fee) = if pps_gross > pps_prev {
            let mgmt = (aum.0 as u128 * self.cfg.mgmt_fee_bps_annual as u128
                * self.cfg.round_ms as u128)
                / (10_000u128 * YEAR_MS as u128);
            let perf =
                (premium_in_underlying.0 as u128 * self.cfg.perf_fee_bps as u128) / 10_000;
            let baseline = (shares.0 as u128 * pps_prev.0) / PPS_SCALE;
            let profit = aum.0 as u128 - baseline;
            let mgmt_charged = mgmt.min(profit);
            let perf_charged = perf.min(profit - mgmt_charged);
            (UnderlyingAmt(mgmt_charged as u64), UnderlyingAmt(perf_charged as u64))
        } else {
            (UnderlyingAmt::ZERO, UnderlyingAmt::ZERO)
        };
        let fees = mgmt_fee + perf_fee;
        self.fees_paid += fees;

        // Lock the round price.
        let net_aum = aum - fees;
        let pps = if shares.0 == 0 {
            pps_prev
        } else {
            Pps((net_aum.0 as u128 * PPS_SCALE) / shares.0 as u128)
        };
        debug_assert_eq!(self.pps.len(), round as usize, "pps[r] is set exactly once");
        self.pps.push(pps);

        self.deployable = net_aum;

        // Queues, in order: withdrawals first, then deposits, both at
        // pps[round].
        let shares_burned = self.queued_withdraw_shares;
        let withdrawals_owed = pps.shares_to_underlying(shares_burned);
        self.deployable -= withdrawals_owed;
        self.withdrawal_pool += withdrawals_owed;
        self.queued_withdraw_shares = ShareAmt::ZERO;

        let deposits_processed = self.pending_deposits;
        let shares_minted = pps.underlying_to_shares(deposits_processed);
        self.deployable += deposits_processed;
        self.pending_deposits = UnderlyingAmt::ZERO;
        self.share_supply += shares_minted;
        self.claimable_shares += shares_minted;

        self.round += 1;

        FinalizeOutcome {
            round,
            pps,
            aum,
            shares,
            mgmt_fee,
            perf_fee,
            withdrawals_owed,
            shares_burned,
            deposits_processed,
            shares_minted,
        }
    }

    // ---- views ----

    pub fn round(&self) -> u64 {
        self.round
    }

    pub fn pps_at(&self, round: u64) -> Result<Pps, LedgerError> {
        self.pps
            .get(round as usize)
            .copied()
            .ok_or(LedgerError::RoundNotPriced(round))
    }

    pub fn deployable(&self) -> UnderlyingAmt {
        self.deployable
    }

    pub fn pending_deposits(&self) -> UnderlyingAmt {
        self.pending_deposits
    }

    pub fn withdrawal_pool(&self) -> UnderlyingAmt {
        self.withdrawal_pool
    }

    pub fn fees_paid(&self) -> UnderlyingAmt {
        self.fees_paid
    }

    /// Outstanding shares (incl. unclaimed), excluding queued escrow.
    pub fn share_supply(&self) -> ShareAmt {
        self.share_supply
    }

    pub fn queued_withdraw_shares(&self) -> ShareAmt {
        self.queued_withdraw_shares
    }

    /// Total underlying the ledger is holding across all buckets.
    pub fn total_assets(&self) -> UnderlyingAmt {
        self.deployable + self.pending_deposits + self.withdrawal_pool
    }
}

/// Convert a settlement amount to underlying at an execution price given as
/// settlement-per-underlying at scale 0 (floor — the swap can only round
/// against the vault). Shared by the engine's premium/exercise-proceeds
/// conversions so the perf-fee base and the swap output use one rule.
pub fn settlement_to_underlying(amount: SettleAmt, price: f64) -> UnderlyingAmt {
    assert!(price > 0.0 && price.is_finite(), "bad conversion price");
    UnderlyingAmt((amount.0 as f64 / price).floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_FEES: LedgerConfig = LedgerConfig {
        mgmt_fee_bps_annual: 0,
        perf_fee_bps: 0,
        round_ms: WEEK_MS,
    };
    const RIBBON_FEES: LedgerConfig = LedgerConfig {
        mgmt_fee_bps_annual: 200,
        perf_fee_bps: 1000,
        round_ms: WEEK_MS,
    };
    const WEEK_MS: u64 = 604_800_000;

    /// 1000 SUI in MIST — large vs PPS_SCALE so floor dust is sub-unit.
    const U1000: u64 = 1_000_000_000_000;

    fn genesis_with(amount: u64) -> (Ledger, DepositReceipt) {
        let mut l = Ledger::new(NO_FEES);
        let r = l.deposit(UnderlyingAmt(amount));
        let out = l.finalize_round(UnderlyingAmt::ZERO, UnderlyingAmt::ZERO);
        assert_eq!(out.round, 0);
        assert_eq!(out.pps, Pps::ONE);
        (l, r)
    }

    #[test]
    fn genesis_finalize_activates_round_1() {
        let (l, receipt) = genesis_with(U1000);
        assert_eq!(l.round(), 1);
        assert_eq!(l.pps_at(0), Ok(Pps::ONE));
        assert_eq!(l.deployable(), UnderlyingAmt(U1000));
        assert_eq!(l.pending_deposits(), UnderlyingAmt::ZERO);
        assert_eq!(l.share_supply(), ShareAmt(U1000));
        assert_eq!(receipt.round, 1);
    }

    #[test]
    fn receipt_round_convention_converts_at_prior_round_pps() {
        // The one dedicated test for the off-by-one rule (doc 03 §7.4): a
        // round-r receipt enters at the start of round r and converts at
        // pps[r − 1].
        let (mut l, genesis_receipt) = genesis_with(U1000);

        // Deposit during round 1 → receipt round 2.
        let receipt = l.deposit(UnderlyingAmt(U1000));
        assert_eq!(receipt.round, 2);
        // Not claimable yet: pps[1] doesn't exist.
        assert_eq!(l.claim_shares(receipt), Err(LedgerError::RoundNotPriced(1)));

        // Round 1 ends +10%: pps[1] = 1.1.
        l.finalize_round(UnderlyingAmt(U1000 + U1000 / 10), UnderlyingAmt::ZERO);
        let pps1 = l.pps_at(1).unwrap();
        assert_eq!(pps1, Pps(PPS_SCALE / 10 * 11));

        // Genesis receipt (round 1) converts at pps[0] = 1.0; the round-2
        // receipt converts at pps[1] = 1.1.
        assert_eq!(l.claim_shares(genesis_receipt).unwrap(), ShareAmt(U1000));
        let shares = l.claim_shares(receipt).unwrap();
        assert_eq!(shares, pps1.underlying_to_shares(UnderlyingAmt(U1000)));
        assert_eq!(shares, ShareAmt(909_090_909_090));
    }

    #[test]
    fn pending_deposits_are_inert_for_the_current_round() {
        // Invariant 6: round P&L accrues only to live shares. A deposit
        // queued during a +10% round buys at the post-profit price.
        let (mut l, _) = genesis_with(U1000);
        l.deposit(UnderlyingAmt(U1000));

        // aum excludes pending; live shares = U1000.
        let out = l.finalize_round(UnderlyingAmt(U1000 + U1000 / 10), UnderlyingAmt::ZERO);
        assert_eq!(out.shares, ShareAmt(U1000));
        assert_eq!(out.pps, Pps(PPS_SCALE / 10 * 11));
        // The pending depositor got 909.09…, not 1000, shares: no free P&L.
        assert_eq!(out.shares_minted, ShareAmt(909_090_909_090));
        // Both cohorts' value adds up to the whole pot (floor dust ≤ 1).
        let total = out.pps.shares_to_underlying(ShareAmt(U1000) + out.shares_minted);
        assert!(U1000 * 2 + U1000 / 10 - total.0 <= 1);
    }

    #[test]
    fn deposit_then_withdraw_after_one_round_yields_pps_ratio() {
        // Invariant 5: a depositor who rides exactly one round receives
        // amount × pps[r] / pps[r−1] — the queues neither add nor leak.
        let (mut l, receipt) = genesis_with(U1000);
        let shares = l.claim_shares(receipt).unwrap();

        // Round 1: +7%.
        let aum1 = U1000 + U1000 * 7 / 100;
        l.finalize_round(UnderlyingAmt(aum1), UnderlyingAmt::ZERO);

        // Withdraw everything during round 2; flat round.
        let wreceipt = l.initiate_withdraw(shares);
        assert_eq!(wreceipt.round, 2);
        let out = l.finalize_round(UnderlyingAmt(aum1), UnderlyingAmt::ZERO);
        assert_eq!(out.pps, l.pps_at(1).unwrap()); // flat round, same pps

        let paid = l.complete_withdraw(wreceipt).unwrap();
        let expected =
            (U1000 as u128 * l.pps_at(2).unwrap().0 / l.pps_at(0).unwrap().0) as u64;
        assert!(expected - paid.0 <= 1, "paid {} vs expected {}", paid.0, expected);
        // Everything accounted: pool drained to dust, deployable holds dust.
        assert!(l.withdrawal_pool().0 <= 1);
        assert!(l.deployable().0 <= aum1 - paid.0);
    }

    #[test]
    fn queued_withdraw_shares_stay_exposed_to_the_round() {
        // Ribbon semantics (doc 03 §5.3): initiating a withdrawal does not
        // exit the current round — the escrowed shares settle at this
        // round's pps, profit included.
        let (mut l, receipt) = genesis_with(U1000);
        let shares = l.claim_shares(receipt).unwrap();
        let w = l.initiate_withdraw(shares);
        assert_eq!(w.round, 1);

        // Round 1 ends +10%; the queued shares are the only live shares.
        let out = l.finalize_round(UnderlyingAmt(U1000 + U1000 / 10), UnderlyingAmt::ZERO);
        assert_eq!(out.shares, ShareAmt(U1000));
        assert_eq!(out.shares_burned, ShareAmt(U1000));
        let paid = l.complete_withdraw(w).unwrap();
        assert_eq!(paid, UnderlyingAmt(U1000 + U1000 / 10)); // full profit captured
    }

    #[test]
    fn fees_charged_only_on_profitable_rounds() {
        let mut l = Ledger::new(RIBBON_FEES);
        let r = l.deposit(UnderlyingAmt(U1000));
        l.finalize_round(UnderlyingAmt::ZERO, UnderlyingAmt::ZERO);
        l.claim_shares(r).unwrap();

        // Losing round: spot dropped, no fees even though premium came in.
        let out = l.finalize_round(
            UnderlyingAmt(U1000 - U1000 / 10),
            UnderlyingAmt(U1000 / 100),
        );
        assert_eq!(out.mgmt_fee, UnderlyingAmt::ZERO);
        assert_eq!(out.perf_fee, UnderlyingAmt::ZERO);

        // Profitable round: both fees, exact formulas.
        let aum = U1000;
        let premium_u = U1000 / 50; // 2% premium
        let out = l.finalize_round(UnderlyingAmt(aum), UnderlyingAmt(premium_u));
        let expected_mgmt =
            (aum as u128 * 200 * WEEK_MS as u128 / (10_000 * YEAR_MS as u128)) as u64;
        let expected_perf = premium_u / 10;
        assert_eq!(out.mgmt_fee, UnderlyingAmt(expected_mgmt));
        assert_eq!(out.perf_fee, UnderlyingAmt(expected_perf));
        assert_eq!(l.fees_paid(), UnderlyingAmt(expected_mgmt + expected_perf));
    }

    #[test]
    fn fee_cap_never_pushes_pps_below_prev() {
        // Round profit smaller than mgmt + perf: fees clamp to profit and
        // pps stays at pps_prev (to the 1-ulp double-floor).
        let mut l = ledger_with_flat_history();
        let pps_prev = l.pps_at(1).unwrap();
        let tiny_profit = 1_000u64; // 1000 units on a 1e12 base
        let aum = l.deployable().0 + tiny_profit;
        let out = l.finalize_round(UnderlyingAmt(aum), UnderlyingAmt(U1000 / 50));
        assert_eq!(out.fees(), UnderlyingAmt(tiny_profit));
        // mgmt first, perf absorbs the cap.
        assert_eq!(out.mgmt_fee, UnderlyingAmt(tiny_profit));
        assert_eq!(out.perf_fee, UnderlyingAmt::ZERO);
        assert!(out.pps.0 + 1 >= pps_prev.0, "pps {} vs prev {}", out.pps.0, pps_prev.0);
        assert!(out.pps <= pps_prev);
    }

    fn ledger_with_flat_history() -> Ledger {
        let mut l = Ledger::new(RIBBON_FEES);
        let r = l.deposit(UnderlyingAmt(U1000));
        l.finalize_round(UnderlyingAmt::ZERO, UnderlyingAmt::ZERO); // round 0
        l.claim_shares(r).unwrap();
        l.finalize_round(UnderlyingAmt(U1000), UnderlyingAmt::ZERO); // flat round 1
        l
    }

    #[test]
    fn zero_share_rounds_keep_pps_and_park_dust() {
        // All shares withdrawn; residual dust keeps compounding nowhere —
        // pps freezes and the vault stays finalizable (liveness, doc 03
        // §11.8).
        let (mut l, receipt) = genesis_with(U1000);
        let shares = l.claim_shares(receipt).unwrap();
        let w = l.initiate_withdraw(shares);
        l.finalize_round(UnderlyingAmt(U1000), UnderlyingAmt::ZERO);
        l.complete_withdraw(w).unwrap();

        let pps_before = l.pps_at(1).unwrap();
        let out = l.finalize_round(l.deployable(), UnderlyingAmt::ZERO);
        assert_eq!(out.shares, ShareAmt::ZERO);
        assert_eq!(out.pps, pps_before);
        assert_eq!(out.fees(), UnderlyingAmt::ZERO);
    }

    #[test]
    fn instant_withdraw_only_before_round_starts() {
        let mut l = Ledger::new(NO_FEES);
        let r1 = l.deposit(UnderlyingAmt(U1000));
        let r2 = l.deposit(UnderlyingAmt(U1000));
        // Cancel r1 while still pending: full refund.
        assert_eq!(l.instant_withdraw_pending(r1), Ok(UnderlyingAmt(U1000)));
        // Round starts; r2 is locked in.
        l.finalize_round(UnderlyingAmt::ZERO, UnderlyingAmt::ZERO);
        assert_eq!(
            l.instant_withdraw_pending(r2),
            Err(LedgerError::DepositRoundStarted)
        );
        assert_eq!(l.deployable(), UnderlyingAmt(U1000));
    }

    #[test]
    fn worked_round_math_from_doc_03_section_8() {
        // The normative example: round 5 starts with pps[4] = 1.05, 1000
        // SUI deployable against 952.380952380 shares; the round ends at
        // aum = 1007.4 SUI having collected premium worth 12 SUI, with
        // 2/10 fees. Golden values below computed by hand from §7.4.
        let supply = ShareAmt(952_380_952_380);
        let mut l = Ledger::for_test_with_state(
            RIBBON_FEES,
            vec![
                Pps::ONE,
                Pps::ONE,
                Pps::ONE,
                Pps::ONE,
                Pps(1_050_000_000_000), // pps[4] = 1.05
            ],
            supply,
            UnderlyingAmt(1_000_000_000_000),
        );
        assert_eq!(l.round(), 5);

        let aum5: u64 = 1_007_400_000_000;
        let premium_u: u64 = 12_000_000_000;
        let out = l.finalize_round(UnderlyingAmt(aum5), UnderlyingAmt(premium_u));

        // mgmt = floor(1007.4e9 × 200 × WEEK / (10⁴ × YEAR)) — exact.
        assert_eq!(out.mgmt_fee, UnderlyingAmt(386_400_000));
        // perf = 12e9 × 1000 / 10⁴.
        assert_eq!(out.perf_fee, UnderlyingAmt(1_200_000_000));
        // profit = 1007.4e9 − floor(952.38…e9 × 1.05) = 7_400_000_001:
        // fees uncapped.
        assert_eq!(out.fees(), UnderlyingAmt(1_586_400_000));
        // pps[5] = floor((aum − fees) × 1e12 / supply).
        let expected_pps =
            (aum5 as u128 - 1_586_400_000) * PPS_SCALE / supply.0 as u128;
        assert_eq!(out.pps, Pps(expected_pps));
        // ≈ 1.05610: profitable round, pps advanced past 1.05.
        assert!(out.pps.0 > 1_050_000_000_000 && out.pps.0 < 1_060_000_000_000);
    }

    #[test]
    fn randomized_multi_round_conservation() {
        // Doc 06 §9.2: every underlying unit traces through the ledger
        // with exact integer equality, across random deposit/withdraw/
        // P&L sequences.
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};

        for seed in 0..20u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut l = Ledger::new(RIBBON_FEES);
            let mut receipts: Vec<DepositReceipt> = Vec::new();
            let mut wreceipts: Vec<WithdrawReceipt> = Vec::new();
            let mut held_shares = ShareAmt::ZERO;

            let mut total_in: u128 = 0; // deposits
            let mut total_out: u128 = 0; // withdrawals + refunds
            let mut total_pnl: i128 = 0; // engine-injected round P&L
            let mut fees: u128 = 0;

            for _ in 0..40 {
                // Random deposits.
                for _ in 0..rng.gen_range(0..3) {
                    let amt = rng.gen_range(1..=U1000);
                    receipts.push(l.deposit(UnderlyingAmt(amt)));
                    total_in += amt as u128;
                }
                // Random instant cancel.
                if !receipts.is_empty() && rng.gen_bool(0.2) {
                    let r = receipts.swap_remove(rng.gen_range(0..receipts.len()));
                    if let Ok(refund) = l.instant_withdraw_pending(r) {
                        total_out += refund.0 as u128;
                    } else {
                        receipts.push(r);
                    }
                }
                // Claim what's claimable.
                receipts.retain(|r| match l.claim_shares(*r) {
                    Ok(s) => {
                        held_shares += s;
                        false
                    }
                    Err(_) => true,
                });
                // Random withdrawal initiation.
                if held_shares.0 > 0 && rng.gen_bool(0.3) {
                    let take = ShareAmt(rng.gen_range(1..=held_shares.0));
                    held_shares -= take;
                    wreceipts.push(l.initiate_withdraw(take));
                }
                // Round P&L: −10%..+10% on deployable.
                let dep = l.deployable().0 as i128;
                let pnl = dep * rng.gen_range(-100..=100) / 1000;
                total_pnl += pnl;
                let aum = UnderlyingAmt((dep + pnl) as u64);
                let premium = UnderlyingAmt(rng.gen_range(0..=U1000 / 100));
                let out = l.finalize_round(aum, premium);
                fees += out.fees().0 as u128;
                // Complete what's payable.
                wreceipts.retain(|w| match l.complete_withdraw(*w) {
                    Ok(paid) => {
                        total_out += paid.0 as u128;
                        false
                    }
                    Err(_) => true,
                });
            }

            // Exact conservation: in − out − fees + pnl == still inside.
            let inside = l.total_assets().0 as u128;
            let expected = (total_in as i128 + total_pnl
                - total_out as i128
                - fees as i128) as u128;
            assert_eq!(inside, expected, "seed {seed}");

            // Withdrawal pool stays solvent for outstanding receipts.
            let owed: u128 = wreceipts
                .iter()
                .filter_map(|w| {
                    l.pps_at(w.round)
                        .ok()
                        .map(|p| p.shares_to_underlying(w.shares).0 as u128)
                })
                .sum();
            assert!(l.withdrawal_pool().0 as u128 >= owed, "seed {seed}");
        }
    }

    #[test]
    fn settlement_conversion_floors() {
        assert_eq!(
            settlement_to_underlying(SettleAmt(1_000), 3.0),
            UnderlyingAmt(333)
        );
        assert_eq!(
            settlement_to_underlying(SettleAmt(0), 3.0),
            UnderlyingAmt(0)
        );
    }
}
