#[test_only]
module options_protocol::rfq_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::admin;
use options_protocol::bucket::{Self, Bucket};
use options_protocol::position::{Self, Position};
use options_protocol::quote;
use options_protocol::rfq::{Self, RfqAuction};
use options_protocol::test_helpers::{Self as th, BTC, USDC, CALL};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 10_000_000;

// Comfortable auction shape: deadline at 400k, max extension 100k, so the
// hard deadline (500k) + the 600k settle buffer fits the 10M expiry.
const DURATION_MS: u64 = 400_000;
const SNIPE_WINDOW_MS: u64 = 60_000;
const SNIPE_EXTENSION_MS: u64 = 120_000;
const MAX_EXTENSION_MS: u64 = 100_000;
const MIN_INCREMENT_BPS: u64 = 500; // 5% for easy numbers

fun setup(scenario: &mut Scenario): sui::clock::Clock {
    let clock = th::init_protocol(scenario);
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    clock
}

fun seller(): address { th::writer_addr() }
fun bidder_a(): address { th::trader_mm_addr() }
fun bidder_b(): address { th::writer_mm_addr() }

fun origin_id(): ID { object::id_from_address(@0xABCD) }

/// Seller escrows `amount` underlying into a fresh auction with the
/// standard params; outputs routed back to the seller.
fun create_auction(scenario: &mut Scenario, clock: &sui::clock::Clock, amount: u64, reserve: u64) {
    ts::next_tx(scenario, seller());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    rfq::create<BTC, USDC, CALL>(
        &b,
        coin::mint_for_testing<BTC>(amount, scenario.ctx()),
        reserve,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        seller(),
        seller(),
        origin_id(),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
}

fun place_bid(
    scenario: &mut Scenario,
    clock: &sui::clock::Clock,
    bidder: address,
    premium: u64,
) {
    ts::next_tx(scenario, bidder);
    let mut a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(scenario);
    rfq::bid<BTC, USDC, CALL>(
        &mut a,
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        bidder,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
}

// --- create ---

#[test]
fun test_create_records_params_and_escrows() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    ts::next_tx(&mut scenario, seller());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    assert!(rfq::amount(&a) == 100, 0);
    assert!(rfq::reserve_premium(&a) == 1_000, 0);
    assert!(rfq::deadline_ms(&a) == DURATION_MS, 0);
    assert!(rfq::max_deadline_ms(&a) == DURATION_MS + MAX_EXTENSION_MS, 0);
    assert!(rfq::best_premium(&a) == 0, 0);
    assert!(rfq::best_bidder(&a).is_none(), 0);
    assert!(rfq::origin(&a) == origin_id(), 0);
    ts::return_shared(a);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 21, location = options_protocol::rfq)] // zero_amount
fun test_create_zero_amount_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 0, 1_000);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 34, location = options_protocol::rfq)] // rfq_duration_too_short
fun test_create_duration_too_short_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    ts::next_tx(&mut scenario, seller());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    rfq::create<BTC, USDC, CALL>(
        &b,
        coin::mint_for_testing<BTC>(100, scenario.ctx()),
        1_000,
        299_999, // < 5 minutes
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        seller(),
        seller(),
        origin_id(),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 33, location = options_protocol::rfq)] // rfq_too_close_to_expiry
fun test_create_too_close_to_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    // max_deadline = now + 400k + 100k; buffer 600k ⇒ needs expiry ≥
    // now + 1.1M. Past 8.9M it no longer fits the 10M expiry.
    clock.set_for_testing(8_900_001);
    create_auction(&mut scenario, &clock, 100, 1_000);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 26, location = options_protocol::rfq)] // bucket_invalidated
fun test_create_on_invalidated_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::invalidate_bucket<BTC, USDC, CALL>(&cap, &mut b, b"halt", &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(b);

    create_auction(&mut scenario, &clock, 100, 1_000);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- bid ---

#[test]
fun test_outbid_refunds_previous_bidder() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    place_bid(&mut scenario, &clock, bidder_a(), 1_000);
    // 5% increment ⇒ floor is 1_050.
    place_bid(&mut scenario, &clock, bidder_b(), 1_050);
    place_bid(&mut scenario, &clock, bidder_a(), 2_000);

    // bidder_a got their first 1_000 back when b outbid them.
    ts::next_tx(&mut scenario, bidder_a());
    let refund_a = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(refund_a.value() == 1_000, 0);
    coin::burn_for_testing(refund_a);

    // bidder_b got their 1_050 back when a re-bid.
    ts::next_tx(&mut scenario, bidder_b());
    let refund_b = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(refund_b.value() == 1_050, 0);
    coin::burn_for_testing(refund_b);

    ts::next_tx(&mut scenario, seller());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    assert!(rfq::best_premium(&a) == 2_000, 0);
    assert!(rfq::best_bidder(&a).borrow() == bidder_a(), 0);
    ts::return_shared(a);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 31, location = options_protocol::rfq)] // rfq_bid_too_low
fun test_bid_below_reserve_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 999);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 31, location = options_protocol::rfq)] // rfq_bid_too_low
fun test_bid_below_min_increment_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);
    // Floor is 1_000 × 1.05 = 1_050; 1_049 is an insufficient improvement.
    place_bid(&mut scenario, &clock, bidder_b(), 1_049);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 29, location = options_protocol::rfq)] // rfq_closed
fun test_bid_after_deadline_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    clock.set_for_testing(DURATION_MS);
    place_bid(&mut scenario, &clock, bidder_a(), 2_000);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_anti_snipe_extends_and_caps_deadline() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    // Bid inside the snipe window (deadline 400k, window 60k).
    clock.set_for_testing(DURATION_MS - 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);

    ts::next_tx(&mut scenario, seller());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    // Extended to now + 120k = 519k, but capped at max_deadline 500k.
    assert!(rfq::deadline_ms(&a) == DURATION_MS + MAX_EXTENSION_MS, 0);
    ts::return_shared(a);

    // A bid right before the capped deadline cannot extend further.
    clock.set_for_testing(DURATION_MS + MAX_EXTENSION_MS - 1);
    place_bid(&mut scenario, &clock, bidder_b(), 2_000);
    ts::next_tx(&mut scenario, seller());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    assert!(rfq::deadline_ms(&a) == DURATION_MS + MAX_EXTENSION_MS, 0);
    ts::return_shared(a);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_early_bid_does_not_extend_deadline() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    clock.set_for_testing(100_000); // well outside the snipe window
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);

    ts::next_tx(&mut scenario, seller());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    assert!(rfq::deadline_ms(&a) == DURATION_MS, 0);
    ts::return_shared(a);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- settle ---

#[test]
fun test_full_lifecycle_settles_against_best_bid() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);

    // 50 bps protocol fee so the skim shows up.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::set_fee_bps(&cap, &mut config, 50);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(config);

    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);
    place_bid(&mut scenario, &clock, bidder_b(), 2_000);

    // Anyone settles after the deadline (the stranger cranks).
    clock.set_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle<BTC, USDC, CALL>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());

    // Bucket: write executed for the full slice.
    assert!(bucket::total_written(&b) == 100, 0);
    assert!(bucket::call_supply(&b) == 100, 0);
    assert!(bucket::underlying_balance(&b) == 100, 0);
    ts::return_shared(b);
    ts::return_shared(config);

    // Treasury: 50 bps of 2_000 = 10.
    assert!(options_protocol::treasury::balance_of<USDC>(&treasury) == 10, 0);
    ts::return_shared(treasury);

    // Winner holds the option coin for the slice.
    ts::next_tx(&mut scenario, bidder_b());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    assert!(call.value() == 100, 0);
    ts::return_to_sender(&scenario, call);

    // Seller holds the Position covering [0, 100) and the net premium.
    ts::next_tx(&mut scenario, seller());
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == 100, 0);
    ts::return_to_sender(&scenario, pos);
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == 1_990, 0);
    coin::burn_for_testing(net);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 30, location = options_protocol::rfq)] // rfq_not_closed
fun test_settle_before_deadline_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle<BTC, USDC, CALL>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_no_bid_expiry_refunds_underlying() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    clock.set_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle<BTC, USDC, CALL>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());

    // No write happened.
    assert!(bucket::total_written(&b) == 0, 0);
    assert!(bucket::call_supply(&b) == 0, 0);
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    // Underlying came back to the proceeds recipient.
    ts::next_tx(&mut scenario, seller());
    let refund = ts::take_from_sender<Coin<BTC>>(&scenario);
    assert!(refund.value() == 100, 0);
    coin::burn_for_testing(refund);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- settle_expired ---

#[test]
fun test_settle_expired_refunds_both_escrows() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 5_000);

    // Bucket expires before anyone settles (shouldn't happen thanks to
    // the settle buffer, but must be recoverable).
    clock.set_for_testing(EXPIRY_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    rfq::settle_expired<BTC, USDC, CALL>(a, &b, &clock, scenario.ctx());
    assert!(bucket::total_written(&b) == 0, 0);
    ts::return_shared(b);

    // Bid back to the bidder, collateral back to the seller.
    ts::next_tx(&mut scenario, bidder_a());
    let bid_refund = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(bid_refund.value() == 5_000, 0);
    coin::burn_for_testing(bid_refund);
    ts::next_tx(&mut scenario, seller());
    let collateral = ts::take_from_sender<Coin<BTC>>(&scenario);
    assert!(collateral.value() == 100, 0);
    coin::burn_for_testing(collateral);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_settle_expired_on_invalidated_bucket() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 5_000);

    // Invalidation mid-auction: recoverable immediately, no deadline wait.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::invalidate_bucket<BTC, USDC, CALL>(&cap, &mut b, b"halt", &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(b);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    rfq::settle_expired<BTC, USDC, CALL>(a, &b, &clock, scenario.ctx());
    ts::return_shared(b);

    ts::next_tx(&mut scenario, bidder_a());
    let bid_refund = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(bid_refund.value() == 5_000, 0);
    coin::burn_for_testing(bid_refund);
    ts::next_tx(&mut scenario, seller());
    let collateral = ts::take_from_sender<Coin<BTC>>(&scenario);
    assert!(collateral.value() == 100, 0);
    coin::burn_for_testing(collateral);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 9, location = options_protocol::rfq)] // bucket_not_expired
fun test_settle_expired_on_live_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    rfq::settle_expired<BTC, USDC, CALL>(a, &b, &clock, scenario.ctx());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- venue interleaving ---

#[test]
fun test_rfq_interleaves_with_other_venues_consistently() {
    // Signed-quote write, then an RFQ settle, then a collateralized
    // write, all into one bucket: cursor, supply, and redeems stay exact.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    th::create_account(&mut scenario, bidder_a(), th::pubkey_a());

    ts::next_tx(&mut scenario, bidder_a());
    let mut acc = th::take_account(&scenario);
    options_protocol::account::deposit(
        &mut acc,
        coin::mint_for_testing<USDC>(10_000_000, scenario.ctx()),
    );
    ts::return_shared(acc);

    // Venue 1 (signed quote): [0, 100), position → seller, calls → MM.
    ts::next_tx(&mut scenario, seller());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut acc = th::take_account(&scenario);
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&acc),
        bidder_a(),
        object::id(&b),
        100,
        1_000,
        EXPIRY_MS,
        1,
    );
    bucket::execute_write_for_testing<BTC, USDC, CALL>(
        &mut b,
        &config,
        &mut treasury,
        &mut acc,
        coin::mint_for_testing<BTC>(100, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        seller(),
        bidder_a(),
        quote::new_signed_quote(q, vector[]),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(acc);

    // Venue 2 (RFQ): [100, 150), winner bidder_b.
    create_auction(&mut scenario, &clock, 50, 1_000);
    place_bid(&mut scenario, &clock, bidder_b(), 1_500);
    clock.set_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<RfqAuction<BTC, USDC, CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle<BTC, USDC, CALL>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(config);
    ts::return_shared(treasury);

    // Venue 3 (self-write): [150, 200), stranger keeps both legs.
    ts::next_tx(&mut scenario, th::stranger_addr());
    let (pos3, call3) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(position::range_start(&pos3) == 150, 0);

    assert!(bucket::total_written(&b) == 200, 0);
    assert!(bucket::call_supply(&b) == 200, 0);
    assert!(bucket::underlying_balance(&b) == 200, 0);

    // Exercise 130: all of venue 1's range, 30 into the RFQ's range.
    ts::next_tx(&mut scenario, bidder_a());
    let calls_a = ts::take_from_sender<Coin<CALL>>(&scenario);
    let u = bucket::exercise<BTC, USDC, CALL>(
        &mut b,
        calls_a,
        coin::mint_for_testing<USDC>((((100 as u128) * STRIKE) as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(u);
    ts::next_tx(&mut scenario, bidder_b());
    let mut calls_b = ts::take_from_sender<Coin<CALL>>(&scenario);
    let chunk = coin::split(&mut calls_b, 30, scenario.ctx());
    let u = bucket::exercise<BTC, USDC, CALL>(
        &mut b,
        chunk,
        coin::mint_for_testing<USDC>((((30 as u128) * STRIKE) as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(u);
    ts::return_to_sender(&scenario, calls_b);
    assert!(bucket::exercise_cursor(&b) == 130, 0);

    // Past expiry: each position redeems its exact FIFO share.
    clock.set_for_testing(EXPIRY_MS + 1);

    // Seller holds two positions (signed write [0,100) + RFQ [100,150)).
    ts::next_tx(&mut scenario, seller());
    let pos_x = ts::take_from_sender<Position>(&scenario);
    let pos_y = ts::take_from_sender<Position>(&scenario);
    let (pos1, pos2) = if (position::range_start(&pos_x) == 0) {
        (pos_x, pos_y)
    } else {
        (pos_y, pos_x)
    };
    let (u1, s1) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos1, &clock, scenario.ctx());
    assert!(u1.value() == 0, 0);
    assert!(s1.value() == (((100 as u128) * STRIKE) as u64), 0);
    let (u2, s2) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos2, &clock, scenario.ctx());
    assert!(u2.value() == 20, 0);
    assert!(s2.value() == (((30 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(u1);
    coin::burn_for_testing(s1);
    coin::burn_for_testing(u2);
    coin::burn_for_testing(s2);

    // Stranger's self-write is untouched.
    ts::next_tx(&mut scenario, th::stranger_addr());
    let (u3, s3) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos3, &clock, scenario.ctx());
    assert!(u3.value() == 50, 0);
    assert!(s3.value() == 0, 0);
    coin::burn_for_testing(u3);
    coin::burn_for_testing(s3);
    coin::burn_for_testing(call3);

    // Fully drained; outstanding supply = unexercised, unburned calls.
    assert!(bucket::underlying_balance(&b) == 0, 0);
    assert!(bucket::settlement_balance(&b) == 0, 0);
    assert!(bucket::call_supply(&b) == 70, 0); // 20 (bidder_b) + 50 (stranger)
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}
