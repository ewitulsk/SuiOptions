//! Golden-file cross-validation (E1, doc 06 §9.3): the SAME round inputs
//! driven through `vault.move`'s test suite and through the vault-sim
//! `Ledger` must produce identical integers — pps, fees, payouts, share
//! supply, to the unit. Each test mirrors a Move test by name; the shared
//! constants ARE the golden file. If either side changes a number, the
//! pair must be updated together:
//!
//!   - [`full_round_lifecycle_with_partial_exercise`] ↔
//!     `contracts/tests/vault_tests.move::
//!      test_full_round_lifecycle_with_partial_exercise`
//!   - [`fee_cap_clamps_to_round_profit`] ↔
//!     `vault_tests.move::test_fee_cap_clamps_to_round_profit`
//!
//! The Move side exercises the real bucket/RFQ/swap plumbing; this side
//! checks the accounting kernel reproduces every derived value from the
//! same primitives (slice premium, exercise proceeds, swap fill).

use vault_sim::ledger::{Ledger, LedgerConfig};
use vault_sim::types::{Pps, ShareAmt, UnderlyingAmt, PPS_SCALE};

/// vault_tests.move test-harness config: 2%/yr mgmt, 10% perf, weekly
/// rounds (EXPIRY_MS = 604_800_000).
fn cfg() -> LedgerConfig {
    LedgerConfig {
        mgmt_fee_bps_annual: 200,
        perf_fee_bps: 1_000,
        round_ms: 604_800_000,
    }
}

fn u(v: u64) -> UnderlyingAmt {
    UnderlyingAmt(v)
}

#[test]
fn full_round_lifecycle_with_partial_exercise() {
    let mut l = Ledger::new(cfg());

    // Genesis: user A queues 1_000; the first finalize activates round 1
    // at pps[0] = 1.0 and mints 1_000 shares.
    let receipt_a = l.deposit(u(1_000));
    assert_eq!(receipt_a.round, 1);
    let genesis = l.finalize_round(u(0), u(0));
    assert_eq!(genesis.pps, Pps::ONE);
    assert_eq!(genesis.deposits_processed, u(1_000));
    assert_eq!(genesis.shares_minted, ShareAmt(1_000));
    assert_eq!(l.round(), 1);
    assert_eq!(l.deployable(), u(1_000));
    let shares_a = l.claim_shares(receipt_a).unwrap();
    assert_eq!(shares_a, ShareAmt(1_000));

    // Round 1, as the Move test plays it out on real objects:
    //   - 600 sold into one RFQ slice; MM wins at 30_000 settlement.
    //   - MM exercises 200 of 600 calls at strike 50_000 settlement/unit.
    //   - redeem returns 400 unsold-slice underlying; deployable = 800.
    //   - proceeds = 30_000 premium + 200 × 50_000 exercise = 10_030_000.
    //   - the swap fills 10_030_000 settlement for 210 underlying (Pyth
    //     fair 211 at spot 47_619, min after 50 bps slippage = 209).
    let premium_settlement: u64 = 30_000;
    let exercise_proceeds: u64 = 200 * 50_000;
    let swap_settlement_out = premium_settlement + exercise_proceeds;
    assert_eq!(swap_settlement_out, 10_030_000);
    let swap_underlying_in: u64 = 210;
    let deployable_after_redeem: u64 = 800; // 400 unsold + 400 OTM return
    let aum = deployable_after_redeem + swap_underlying_in;
    assert_eq!(aum, 1_010);

    // Premium in underlying at the round's realized swap rate — the perf
    // fee base (`vault.move::finalize_round_internal`): floor(30_000 ×
    // 210 / 10_030_000) = 0.
    let premium_underlying =
        (premium_settlement as u128 * swap_underlying_in as u128) / swap_settlement_out as u128;
    assert_eq!(premium_underlying, 0);

    let r1 = l.finalize_round(u(aum), u(premium_underlying as u64));
    // pps[1] = 1.01 exactly; fees too small to register at this scale.
    assert_eq!(r1.pps, Pps(PPS_SCALE / 100 * 101));
    assert_eq!(r1.fees(), u(0));
    assert_eq!(l.round(), 2);

    // Round 2 idle: A queues a 500-share withdrawal (still exposed to
    // round 2's flat P&L), B queues a 202 deposit for round 3.
    let wreceipt = l.initiate_withdraw(ShareAmt(500));
    assert_eq!(wreceipt.round, 2);
    let receipt_b = l.deposit(u(202));
    assert_eq!(receipt_b.round, 3);

    // No bucket selected ⇒ aum is just the deployable 1_010.
    let r2 = l.finalize_round(u(1_010), u(0));
    assert_eq!(r2.pps, Pps(PPS_SCALE / 100 * 101));
    assert_eq!(r2.fees(), u(0));
    assert_eq!(r2.withdrawals_owed, u(505)); // 500 × 1.01
    assert_eq!(r2.shares_burned, ShareAmt(500));
    assert_eq!(r2.deposits_processed, u(202));
    assert_eq!(r2.shares_minted, ShareAmt(200)); // 202 / 1.01
    assert_eq!(l.withdrawal_pool(), u(505));
    assert_eq!(l.share_supply(), ShareAmt(700)); // 1000 − 500 + 200

    // A's payout: exactly amount × pps[2]/pps[0] — the queues neither
    // add nor leak value (invariant 5).
    let paid = l.complete_withdraw(wreceipt).unwrap();
    assert_eq!(paid, u(505));
}

#[test]
fn fee_cap_clamps_to_round_profit() {
    let mut l = Ledger::new(cfg());

    // Genesis: 1e12 deposited.
    let r = l.deposit(u(1_000_000_000_000));
    l.finalize_round(u(0), u(0));
    let shares = l.claim_shares(r).unwrap();
    assert_eq!(shares, ShareAmt(1_000_000_000_000));

    // Round 1 (Move side): a 1M slice sells at exactly the 47_619_000
    // reserve; the OTM expiry returns everything; the swap converts the
    // premium at the Pyth-fair 1_000 underlying. Profit = 1_000, dwarfed
    // by the round's mgmt accrual (≈ 3.8e8).
    let premium_underlying: u64 = 1_000;
    let aum: u64 = 1_000_000_000_000 + 1_000;

    let out = l.finalize_round(u(aum), u(premium_underlying));
    // Fees clamp to exactly the profit, management first, perf zeroed —
    // the treasury's 1_000 in the Move test.
    assert_eq!(out.mgmt_fee, u(1_000));
    assert_eq!(out.perf_fee, u(0));
    assert_eq!(l.fees_paid(), u(1_000));
    // pps holds at 1.0 to the 1-ulp double-floor.
    assert!(out.pps.get() <= PPS_SCALE && out.pps.get() + 1 >= PPS_SCALE);
}
