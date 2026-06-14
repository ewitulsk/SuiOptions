#[test_only]
module options_protocol::vault_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::bucket::{Self, Bucket};
use options_protocol::rfq::{Self, RfqAuction};
use options_protocol::swap_auction::{Self, SwapAuction};
use options_protocol::test_helpers::{Self as th, BTC, USDC, CALL};
use options_protocol::vault::{Self, Vault, DepositReceipt, WithdrawReceipt};

/// The vault's share coin for tests.
public struct VSHARE has drop {}

const PPS_SCALE: u128 = 1_000_000_000_000;

// Bucket: strike 50_000 settlement-units per underlying-unit (scale 0),
// expiring in exactly 7 days.
const STRIKE: u128 = 50_000;
const EXPIRY_MS: u64 = 604_800_000;

// Pyth cross at the oracle's 1e12 scale: 47_619 settlement per
// underlying ⇒ the 50_000 strike sits 5% over spot, inside the
// [3%, 60%] band.
const SPOT: u128 = 47_619_000_000_000_000;
const SPOT_SCALE: u8 = 12;

const WEEK_MS: u64 = 604_800_000;
const DAY_MS: u64 = 86_400_000;

fun user_a(): address { th::writer_addr() }
fun user_b(): address { th::trader_addr() }
fun mm(): address { th::trader_mm_addr() }
fun keeper(): address { th::stranger_addr() }

fun default_config(): vault::VaultConfig {
    vault::new_config(
        200, // mgmt 2%/yr
        1_000, // perf 10%
        WEEK_MS,
        12 * 60 * 60 * 1_000, // 12h selling window
        300, // strike ≥ 1.03 × spot
        6_000, // strike ≤ 1.60 × spot
        3 * DAY_MS,
        9 * DAY_MS,
        10, // reserve ≥ 10 bps of notional
        1_000_000_000_000, // max slice
        4, // max open rfqs
        400_000, // rfq duration
        60_000,
        120_000,
        100_000,
        500, // 5% min increment
        false, // hold_premium_in_settlement
        50, // 50 bps swap band / reserve slippage
        b"underlying-feed",
        b"settlement-feed",
        60,
        100,
        9,
        6,
    )
}

/// Protocol + bucket + vault, admin-owned caps shared. Clock at 0.
fun setup(scenario: &mut Scenario): sui::clock::Clock {
    let clock = th::init_protocol(scenario);
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, 0);

    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let tcap = coin::create_treasury_cap_for_testing<VSHARE>(scenario.ctx());
    vault::create_vault<BTC, USDC, VSHARE>(&cap, tcap, default_config(), scenario.ctx());
    th::return_admin_cap(scenario, cap);
    clock
}

fun take_vault(scenario: &Scenario): Vault<BTC, USDC, VSHARE> {
    ts::take_shared<Vault<BTC, USDC, VSHARE>>(scenario)
}

fun deposit_as(
    scenario: &mut Scenario,
    who: address,
    amount: u64,
): DepositReceipt {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let receipt = vault::deposit(
        &mut v,
        coin::mint_for_testing<BTC>(amount, scenario.ctx()),
        scenario.ctx(),
    );
    ts::return_shared(v);
    receipt
}

fun finalize_as(scenario: &mut Scenario, who: address, clock: &sui::clock::Clock) {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let mut treasury = th::take_treasury(scenario);
    vault::finalize_round_with_spot_for_testing(
        &mut v,
        &mut treasury,
        SPOT,
        SPOT_SCALE,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(v);
    ts::return_shared(treasury);
}

fun select_as(scenario: &mut Scenario, who: address, clock: &sui::clock::Clock) {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    vault::select_bucket_with_spot_for_testing(&mut v, &b, SPOT, SPOT_SCALE, clock);
    ts::return_shared(v);
    ts::return_shared(b);
}

fun open_rfq_as(
    scenario: &mut Scenario,
    who: address,
    slice: u64,
    clock: &sui::clock::Clock,
): ID {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    let id = vault::open_rfq_with_spot_for_testing(
        &mut v,
        &b,
        slice,
        SPOT,
        SPOT_SCALE,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(v);
    ts::return_shared(b);
    id
}

fun bid_as(
    scenario: &mut Scenario,
    who: address,
    rfq_id: ID,
    premium: u64,
    clock: &sui::clock::Clock,
) {
    ts::next_tx(scenario, who);
    let mut a = ts::take_shared_by_id<RfqAuction<BTC, USDC, CALL>>(scenario, rfq_id);
    rfq::bid(
        &mut a,
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        who,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
}

fun settle_rfq_as(
    scenario: &mut Scenario,
    who: address,
    rfq_id: ID,
    clock: &sui::clock::Clock,
) {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let a = ts::take_shared_by_id<RfqAuction<BTC, USDC, CALL>>(scenario, rfq_id);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    let config = th::take_config(scenario);
    let mut treasury = th::take_treasury(scenario);
    vault::settle_rfq(&mut v, a, &mut b, &config, &mut treasury, clock, scenario.ctx());
    ts::return_shared(v);
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
}

fun crank_redeem_as(scenario: &mut Scenario, who: address, clock: &sui::clock::Clock) {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    vault::crank_redeem(&mut v, &mut b, clock, scenario.ctx());
    ts::return_shared(v);
    ts::return_shared(b);
}

fun open_swap_as(
    scenario: &mut Scenario,
    who: address,
    amount_s: u64,
    clock: &sui::clock::Clock,
): ID {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let id = vault::open_swap_rfq_with_spot_for_testing(
        &mut v,
        amount_s,
        SPOT,
        SPOT_SCALE,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(v);
    id
}

fun swap_bid_as(
    scenario: &mut Scenario,
    who: address,
    swap_id: ID,
    underlying: u64,
    clock: &sui::clock::Clock,
) {
    ts::next_tx(scenario, who);
    let mut a = ts::take_shared_by_id<SwapAuction<BTC, USDC>>(scenario, swap_id);
    swap_auction::bid(
        &mut a,
        coin::mint_for_testing<BTC>(underlying, scenario.ctx()),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
}

/// Settle a swap auction at an explicit spot (lets band-miss tests move
/// the price between open and settle).
fun settle_swap_at(
    scenario: &mut Scenario,
    who: address,
    swap_id: ID,
    spot: u128,
    clock: &sui::clock::Clock,
) {
    ts::next_tx(scenario, who);
    let mut v = take_vault(scenario);
    let a = ts::take_shared_by_id<SwapAuction<BTC, USDC>>(scenario, swap_id);
    vault::settle_swap_rfq_with_spot_for_testing(&mut v, a, spot, SPOT_SCALE, clock, scenario.ctx());
    ts::return_shared(v);
}

/// Convert all the vault's proceeds at the Pyth-bounded auction price: a
/// keeper opens the swap, an MM wins it, the keeper settles after the
/// deadline. Mirrors the real keeper crank pair (doc 04 §2).
fun swap_proceeds_via_auction(
    scenario: &mut Scenario,
    amount_s: u64,
    winning_bid: u64,
    clock: &mut sui::clock::Clock,
) {
    let open_ms = clock.timestamp_ms();
    let swap_id = open_swap_as(scenario, keeper(), amount_s, clock);
    swap_bid_as(scenario, mm(), swap_id, winning_bid, clock);
    clock.set_for_testing(open_ms + 400_000); // past the 400_000 duration
    settle_swap_at(scenario, keeper(), swap_id, SPOT, clock);
}

// ═══════════════════════ the full round loop ═══════════════════════

#[test]
fun test_full_round_lifecycle_with_partial_exercise() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);

    // Genesis: user A queues 1_000; first finalize activates round 1 at
    // pps[0] = 1.0.
    let receipt_a = deposit_as(&mut scenario, user_a(), 1_000);
    assert!(vault::receipt_round(&receipt_a) == 1, 0);
    finalize_as(&mut scenario, keeper(), &clock);

    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    assert!(vault::round(&v) == 1, 0);
    assert!(!vault::is_settling(&v), 0);
    assert!(vault::pps_at(&v, 0) == PPS_SCALE, 0);
    assert!(vault::deployable(&v) == 1_000, 0);
    assert!(vault::share_supply(&v) == 1_000, 0);
    // Claim A's shares at pps[0].
    let shares_a = vault::claim_shares(&mut v, receipt_a, scenario.ctx());
    assert!(shares_a.value() == 1_000, 0);
    ts::return_shared(v);

    // Select the 5%-over-spot bucket; sell 600 via one RFQ slice.
    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_rfqs(&v) == 1, 0);
    assert!(vault::deployable(&v) == 400, 0);
    ts::return_shared(v);
    // Reserve = 10 bps of the 28_571_400 spot notional = 28_571.
    ts::next_tx(&mut scenario, keeper());
    let a = ts::take_shared_by_id<RfqAuction<BTC, USDC, CALL>>(&scenario, rfq_id);
    assert!(rfq::reserve_premium(&a) == 28_571, 0);
    ts::return_shared(a);

    // MM wins at 30_000; anyone settles after the deadline.
    bid_as(&mut scenario, mm(), rfq_id, 30_000, &clock);
    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), rfq_id, &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_rfqs(&v) == 0, 0);
    assert!(vault::pending_positions(&v) == 1, 0);
    assert!(vault::proceeds_settlement(&v) == 30_000, 0);
    assert!(vault::round_premium_collected(&v) == 30_000, 0);
    ts::return_shared(v);

    // MM exercises 200 of its 600 calls before expiry.
    ts::next_tx(&mut scenario, mm());
    let mut calls = ts::take_from_sender<Coin<CALL>>(&scenario);
    assert!(calls.value() == 600, 0);
    let chunk = coin::split(&mut calls, 200, scenario.ctx());
    ts::return_to_sender(&scenario, calls);
    ts::next_tx(&mut scenario, mm());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let u = bucket::exercise(
        &mut b,
        chunk,
        coin::mint_for_testing<USDC>((((200 as u128) * STRIKE) as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(u);
    ts::return_shared(b);

    // Expiry: redeem flips the phase and returns 400 underlying +
    // 200 × K settlement.
    clock.set_for_testing(EXPIRY_MS);
    crank_redeem_as(&mut scenario, keeper(), &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::is_settling(&v), 0);
    assert!(vault::pending_positions(&v) == 0, 0);
    assert!(vault::deployable(&v) == 800, 0); // 400 unsold + 400 back
    assert!(vault::proceeds_settlement(&v) == 30_000 + 200 * 50_000, 0);
    ts::return_shared(v);

    // A swap auction converts all proceeds at the Pyth-bounded price:
    // fair = floor(10_030_000 × 1e12 / 47_619e12) = 210, reserve/min
    // after 50 bps = 208; the MM wins giving 210 underlying.
    swap_proceeds_via_auction(&mut scenario, 10_030_000, 210, &mut clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_swap_rfqs(&v) == 0, 0);
    assert!(vault::proceeds_settlement(&v) == 0, 0);
    assert!(vault::deployable(&v) == 1_010, 0); // 800 + 210 bought
    ts::return_shared(v);

    // Finalize round 1: aum 1_010 over 1_000 shares; fees too small to
    // register at this scale ⇒ pps[1] = 1.01.
    finalize_as(&mut scenario, keeper(), &clock);
    ts::next_tx(&mut scenario, user_a());
    let v = take_vault(&scenario);
    assert!(vault::round(&v) == 2, 0);
    assert!(vault::pps_at(&v, 1) == PPS_SCALE / 100 * 101, 0);
    ts::return_shared(v);

    // Round 2 stays idle: A queues a withdrawal of 500 shares (exposed
    // to round 2's flat P&L), B queues a 202 deposit for round 3.
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    let mut shares_a = shares_a;
    let half = coin::split(&mut shares_a, 500, scenario.ctx());
    let wreceipt = vault::initiate_withdraw(&mut v, half, scenario.ctx());
    assert!(vault::withdraw_receipt_round(&wreceipt) == 2, 0);
    ts::return_shared(v);
    coin::burn_for_testing(shares_a);
    let receipt_b = deposit_as(&mut scenario, user_b(), 202);
    assert!(vault::receipt_round(&receipt_b) == 3, 0);

    // No bucket was selected in round 2 ⇒ finalize is legal immediately.
    finalize_as(&mut scenario, keeper(), &clock);

    // A's withdrawal pays 500 × pps[2] = 505: exactly amount ×
    // pps[2]/pps[0] — the queues neither add nor leak value
    // (invariant 5). B's receipt (round 3) converts at pps[2].
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    assert!(vault::pps_at(&v, 2) == PPS_SCALE / 100 * 101, 0);
    assert!(vault::withdrawal_pool(&v) == 505, 0);
    assert!(vault::share_supply(&v) == 700, 0); // 1000 − 500 + 200
    let paid = vault::complete_withdraw(&mut v, wreceipt, scenario.ctx());
    assert!(paid.value() == 505, 0);
    coin::burn_for_testing(paid);
    let shares_b = vault::claim_shares(&mut v, receipt_b, scenario.ctx());
    assert!(shares_b.value() == 200, 0);
    coin::burn_for_testing(shares_b);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_zero_bid_round_recovers_and_finalizes() {
    // Liveness (doc 03 §11.8): a round where no one bids ends with the
    // collateral refunded and a flat finalize — no admin involved.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    let r = deposit_as(&mut scenario, user_a(), 1_000);
    finalize_as(&mut scenario, keeper(), &clock);
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    coin::burn_for_testing(vault::claim_shares(&mut v, r, scenario.ctx()));
    ts::return_shared(v);

    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);

    // Deadline passes with zero bids; settle refunds the slice.
    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), rfq_id, &clock);
    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::deployable(&v) == 1_000, 0);
    assert!(vault::open_rfqs(&v) == 0, 0);
    ts::return_shared(v);

    // Hold to expiry, finalize flat.
    clock.set_for_testing(EXPIRY_MS);
    finalize_as(&mut scenario, keeper(), &clock);
    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::round(&v) == 2, 0);
    assert!(vault::pps_at(&v, 1) == PPS_SCALE, 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_invalidated_bucket_mid_auction_recovers() {
    // Bucket invalidated while an auction is live: settle_rfq_expired
    // refunds the bid to the bidder and absorbs the collateral; the
    // round finalizes (doc 03 §7.2).
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    let r = deposit_as(&mut scenario, user_a(), 1_000);
    finalize_as(&mut scenario, keeper(), &clock);
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    coin::burn_for_testing(vault::claim_shares(&mut v, r, scenario.ctx()));
    ts::return_shared(v);

    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);
    bid_as(&mut scenario, mm(), rfq_id, 30_000, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::invalidate_bucket(&cap, &mut b, b"halt", &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(b);

    ts::next_tx(&mut scenario, keeper());
    let mut v = take_vault(&scenario);
    let a = ts::take_shared_by_id<RfqAuction<BTC, USDC, CALL>>(&scenario, rfq_id);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    vault::settle_rfq_expired(&mut v, a, &b, &clock, scenario.ctx());
    assert!(vault::deployable(&v) == 1_000, 0);
    assert!(vault::open_rfqs(&v) == 0, 0);
    ts::return_shared(v);
    ts::return_shared(b);

    // The MM got its escrowed bid back.
    ts::next_tx(&mut scenario, mm());
    let refund = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(refund.value() == 30_000, 0);
    coin::burn_for_testing(refund);

    // Round still exits permissionlessly at expiry.
    clock.set_for_testing(EXPIRY_MS);
    finalize_as(&mut scenario, keeper(), &clock);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_fee_cap_clamps_to_round_profit() {
    // Profit smaller than mgmt+perf: fees clamp to profit, mgmt first;
    // pps stays at pps_prev to the 1-ulp double-floor (mirrors
    // vault-sim::ledger::fee_cap_never_pushes_pps_below_prev).
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    let r = deposit_as(&mut scenario, user_a(), 1_000_000_000_000);
    finalize_as(&mut scenario, keeper(), &clock);
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    coin::burn_for_testing(vault::claim_shares(&mut v, r, scenario.ctx()));
    ts::return_shared(v);

    // Round 1: sell a 1M slice; the bid clears the 47_619_000 reserve
    // exactly (10 bps of the 4.76e10 notional) — a premium far below the
    // round's mgmt-fee accrual.
    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 1_000_000, &clock);
    bid_as(&mut scenario, mm(), rfq_id, 47_619_000, &clock);
    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), rfq_id, &clock);

    clock.set_for_testing(EXPIRY_MS);
    crank_redeem_as(&mut scenario, keeper(), &clock); // OTM: all back

    // Swap the premium: fair = floor(47_619_000 × 1e12 / 47_619e12) =
    // 1_000; the MM wins giving fair (min after 50 bps = 995).
    swap_proceeds_via_auction(&mut scenario, 47_619_000, 1_000, &mut clock);

    finalize_as(&mut scenario, keeper(), &clock);

    // aum = 1e12 + 1_000; profit = 1_000; mgmt accrual ≈ 3.8e8 ≫ profit
    // ⇒ fees clamp to exactly the profit (mgmt first, perf zeroed) and
    // pps holds at 1.0 to the 1-ulp double-floor.
    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    let pps1 = vault::pps_at(&v, 1);
    assert!(pps1 <= PPS_SCALE && pps1 + 1 >= PPS_SCALE, 0);
    ts::return_shared(v);
    let treasury = th::take_treasury(&scenario);
    assert!(options_protocol::treasury::balance_of<BTC>(&treasury) == 1_000, 0);
    ts::return_shared(treasury);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// ═══════════════════════ phase / guard aborts ═══════════════════════

#[test]
#[expected_failure(abort_code = 35, location = options_protocol::vault)] // vault_wrong_phase
fun test_select_bucket_before_genesis_finalize_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    select_as(&mut scenario, keeper(), &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 37, location = options_protocol::vault)] // vault_bucket_already_selected
fun test_select_bucket_twice_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 36, location = options_protocol::vault)] // vault_bucket_not_selected
fun test_open_rfq_without_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    open_rfq_as(&mut scenario, keeper(), 100, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 38, location = options_protocol::vault)] // vault_selling_closed
fun test_open_rfq_after_selling_window_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    clock.set_for_testing(12 * 60 * 60 * 1_000); // selling window over
    open_rfq_as(&mut scenario, keeper(), 100, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 45, location = options_protocol::vault)] // vault_slice_too_large
fun test_open_rfq_slice_beyond_deployable_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    open_rfq_as(&mut scenario, keeper(), 1_001, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 40, location = options_protocol::vault)] // vault_rfqs_open
fun test_finalize_with_open_rfq_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    open_rfq_as(&mut scenario, keeper(), 600, &clock);
    clock.set_for_testing(EXPIRY_MS);
    finalize_as(&mut scenario, keeper(), &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 39, location = options_protocol::vault)] // vault_positions_pending
fun test_finalize_with_unredeemed_position_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);
    bid_as(&mut scenario, mm(), rfq_id, 30_000, &clock);
    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), rfq_id, &clock);
    clock.set_for_testing(EXPIRY_MS);
    finalize_as(&mut scenario, keeper(), &clock); // position never redeemed
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 35, location = options_protocol::vault)] // vault_wrong_phase
fun test_finalize_before_expiry_with_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    finalize_as(&mut scenario, keeper(), &clock); // expiry not reached
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 53, location = options_protocol::vault)] // vault_proceeds_unswapped
fun test_finalize_with_unswapped_proceeds_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);
    bid_as(&mut scenario, mm(), rfq_id, 30_000, &clock);
    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), rfq_id, &clock);
    clock.set_for_testing(EXPIRY_MS);
    crank_redeem_as(&mut scenario, keeper(), &clock);
    // hold_premium_in_settlement == false and the premium was never
    // swapped: finalize must refuse.
    finalize_as(&mut scenario, keeper(), &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 43, location = options_protocol::vault)] // vault_strike_out_of_band
fun test_select_bucket_strike_below_band_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    // Spot equal to the strike: 0% over ⇒ below the 3% minimum.
    ts::next_tx(&mut scenario, keeper());
    let mut v = take_vault(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    vault::select_bucket_with_spot_for_testing(
        &mut v,
        &b,
        50_000_000_000_000_000, // spot == strike
        SPOT_SCALE,
        &clock,
    );
    ts::return_shared(v);
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 44, location = options_protocol::vault)] // vault_expiry_out_of_band
fun test_select_bucket_expiry_too_close_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    // 5 days in: only 2 days to expiry < the 3-day minimum lead.
    clock.set_for_testing(5 * DAY_MS);
    select_as(&mut scenario, keeper(), &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 47, location = options_protocol::vault)] // vault_deposits_paused
fun test_paused_deposits_abort() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = take_vault(&scenario);
    vault::pause_deposits(&cap, &mut v);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(v);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 41, location = options_protocol::vault)] // vault_round_not_finalized
fun test_claim_before_round_priced_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    let receipt = deposit_as(&mut scenario, user_a(), 1_000);
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    let shares = vault::claim_shares(&mut v, receipt, scenario.ctx());
    coin::burn_for_testing(shares);
    ts::return_shared(v);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 42, location = options_protocol::vault)] // vault_receipt_round_mismatch
fun test_instant_withdraw_after_round_started_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    let receipt = deposit_as(&mut scenario, user_a(), 1_000);
    finalize_as(&mut scenario, keeper(), &clock); // round 1 starts
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    let coin = vault::instant_withdraw_pending(&mut v, receipt, scenario.ctx());
    coin::burn_for_testing(coin);
    ts::return_shared(v);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_instant_withdraw_before_round_starts_refunds() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    let receipt = deposit_as(&mut scenario, user_a(), 1_000);
    ts::next_tx(&mut scenario, user_a());
    let mut v = take_vault(&scenario);
    let coin = vault::instant_withdraw_pending(&mut v, receipt, scenario.ctx());
    assert!(coin.value() == 1_000, 0);
    assert!(vault::pending_deposits(&v) == 0, 0);
    coin::burn_for_testing(coin);
    ts::return_shared(v);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 48, location = options_protocol::rfq)] // vault_wrong_origin
fun test_generic_settle_rejects_coupled_auction() {
    // The permissionless rfq::settle must not be able to strand a
    // vault-coupled auction's outputs at the vault's ID-address.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);
    let rfq_id = open_rfq_as(&mut scenario, keeper(), 600, &clock);
    bid_as(&mut scenario, mm(), rfq_id, 30_000, &clock);
    clock.set_for_testing(400_000);

    ts::next_tx(&mut scenario, keeper());
    let a = ts::take_shared_by_id<RfqAuction<BTC, USDC, CALL>>(&scenario, rfq_id);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 48, location = options_protocol::vault)] // vault_wrong_origin
fun test_settle_rfq_rejects_foreign_auction() {
    // A standalone (non-vault) auction can't be pushed through the
    // vault's settle path.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();
    finalize_as(&mut scenario, keeper(), &clock);
    select_as(&mut scenario, keeper(), &clock);

    // Standalone auction by user B against the same bucket.
    ts::next_tx(&mut scenario, user_b());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let foreign_id = rfq::create(
        &b,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        1,
        400_000,
        60_000,
        120_000,
        100_000,
        500,
        user_b(),
        user_b(),
        object::id_from_address(user_b()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(b);

    clock.set_for_testing(400_000);
    settle_rfq_as(&mut scenario, keeper(), foreign_id, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 54, location = options_protocol::vault)] // vault_config_invalid
fun test_config_hard_caps_enforced() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = take_vault(&scenario);
    let mut bad = default_config();
    bad = vault::new_config(
        501, // > 5% mgmt hard cap
        1_000,
        WEEK_MS,
        12 * 60 * 60 * 1_000,
        300,
        6_000,
        3 * DAY_MS,
        9 * DAY_MS,
        10,
        1_000_000_000_000,
        4,
        400_000,
        60_000,
        120_000,
        100_000,
        500,
        false,
        50,
        b"underlying-feed",
        b"settlement-feed",
        60,
        100,
        9,
        6,
    );
    let _ = bad;
    vault::update_config(&cap, &mut v, bad);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(v);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_config_update_applies_at_next_finalize() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    deposit_as(&mut scenario, user_a(), 1_000).burn_for_testing();

    // Queue a config change before genesis finalize.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = take_vault(&scenario);
    let new_cfg = vault::new_config(
        0,
        0,
        WEEK_MS,
        12 * 60 * 60 * 1_000,
        100, // wider band: ≥ 1.01 × spot
        6_000,
        3 * DAY_MS,
        9 * DAY_MS,
        10,
        1_000_000_000_000,
        4,
        400_000,
        60_000,
        120_000,
        100_000,
        500,
        false,
        50,
        b"underlying-feed",
        b"settlement-feed",
        60,
        100,
        9,
        6,
    );
    vault::update_config(&cap, &mut v, new_cfg);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(v);

    finalize_as(&mut scenario, keeper(), &clock);

    // Spot only 2% under strike — out of band for the old config (3%),
    // legal under the new one: proves the pending config applied.
    ts::next_tx(&mut scenario, keeper());
    let mut v = take_vault(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    vault::select_bucket_with_spot_for_testing(
        &mut v,
        &b,
        49_000_000_000_000_000,
        SPOT_SCALE,
        &clock,
    );
    assert!(vault::current_bucket(&v).is_some(), 0);
    ts::return_shared(v);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

use fun burn_receipt as DepositReceipt.burn_for_testing;

/// Park an unwanted receipt (tests that don't exercise the claim path).
fun burn_receipt(receipt: DepositReceipt) {
    transfer::public_transfer(receipt, @0x0);
}

// ═══════════════ proceeds-swap auction (doc 03 §7.3) ═══════════════

// fair(amount_s) = floor(amount_s × 1e12 / SPOT); reserve/min = fair ×
// 0.995. At amount_s = 10_030_000 against SPOT: fair = 210, min = 208.
const PROCEEDS: u64 = 10_030_000;

fun fund_proceeds(scenario: &mut Scenario, amount_s: u64) {
    ts::next_tx(scenario, keeper());
    let mut v = take_vault(scenario);
    vault::inject_proceeds_for_testing(
        &mut v,
        coin::mint_for_testing<USDC>(amount_s, scenario.ctx()),
    );
    ts::return_shared(v);
}

#[test]
fun test_swap_auction_fills_in_band() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    fund_proceeds(&mut scenario, PROCEEDS);

    let swap_id = open_swap_as(&mut scenario, keeper(), PROCEEDS, &clock);
    // Reserve floor derived from the open-time Pyth cross.
    ts::next_tx(&mut scenario, keeper());
    let a = ts::take_shared_by_id<SwapAuction<BTC, USDC>>(&scenario, swap_id);
    assert!(swap_auction::reserve_underlying(&a) == 208, 0);
    ts::return_shared(a);

    swap_bid_as(&mut scenario, mm(), swap_id, 210, &clock);
    clock.set_for_testing(400_000);
    settle_swap_at(&mut scenario, keeper(), swap_id, SPOT, &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_swap_rfqs(&v) == 0, 0);
    assert!(vault::proceeds_settlement(&v) == 0, 0);
    assert!(vault::deployable(&v) == 210, 0); // underlying absorbed
    ts::return_shared(v);

    // The winning MM received the settlement.
    ts::next_tx(&mut scenario, mm());
    let won = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(won.value() == PROCEEDS, 0);
    coin::burn_for_testing(won);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_swap_auction_band_miss_refunds() {
    // A bid that clears the open-time reserve but falls out of band once
    // the price moves before settle must not execute: the settlement
    // returns to proceeds and the bidder is refunded.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    fund_proceeds(&mut scenario, PROCEEDS);

    let swap_id = open_swap_as(&mut scenario, keeper(), PROCEEDS, &clock);
    swap_bid_as(&mut scenario, mm(), swap_id, 210, &clock); // clears reserve 208
    clock.set_for_testing(400_000);
    // Settle at a lower spot ⇒ fair jumps to 250, min 248 > the 210 bid.
    settle_swap_at(&mut scenario, keeper(), swap_id, 40_000_000_000_000_000, &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_swap_rfqs(&v) == 0, 0);
    assert!(vault::proceeds_settlement(&v) == PROCEEDS, 0); // restored
    assert!(vault::deployable(&v) == 0, 0); // nothing absorbed
    ts::return_shared(v);

    // The bidder got their underlying back.
    ts::next_tx(&mut scenario, mm());
    let refund = ts::take_from_sender<Coin<BTC>>(&scenario);
    assert!(refund.value() == 210, 0);
    coin::burn_for_testing(refund);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_swap_auction_no_bid_returns_proceeds() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    fund_proceeds(&mut scenario, PROCEEDS);

    let swap_id = open_swap_as(&mut scenario, keeper(), PROCEEDS, &clock);
    clock.set_for_testing(400_000);
    settle_swap_at(&mut scenario, keeper(), swap_id, SPOT, &clock);

    ts::next_tx(&mut scenario, keeper());
    let v = take_vault(&scenario);
    assert!(vault::open_swap_rfqs(&v) == 0, 0);
    assert!(vault::proceeds_settlement(&v) == PROCEEDS, 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 40, location = options_protocol::vault)] // vault_rfqs_open
fun test_finalize_blocked_while_swap_open() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    fund_proceeds(&mut scenario, PROCEEDS);

    let _swap_id = open_swap_as(&mut scenario, keeper(), PROCEEDS, &clock);
    // An open swap auction blocks finalize (genesis round 0 here).
    finalize_as(&mut scenario, keeper(), &clock);

    clock.destroy_for_testing();
    ts::end(scenario);
}
