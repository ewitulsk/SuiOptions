#[test_only]
module options_rfq::rfq_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use auction::auction::{Self as auc, Auction};
use options_core::bucket::Bucket;
use options_core::position::{Self, Position};
use options_core::put_bucket::PutBucket;

use options_rfq::rfq::{Self, CallRfq, PutRfq};
use options_rfq::test_helpers::{Self as th, BTC, USDC, CALL, PUT};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 10_000_000;

const AMOUNT: u64 = 10;
const RESERVE: u64 = 100_000;

// Deadline 400k + max extension 100k + the 600k settle buffer fits the
// 10M expiry comfortably.
const DURATION_MS: u64 = 400_000;
const SNIPE_WINDOW_MS: u64 = 60_000;
const SNIPE_EXTENSION_MS: u64 = 120_000;
const MAX_EXTENSION_MS: u64 = 100_000;
const MIN_INCREMENT_BPS: u64 = 500;

fun origin_id(): ID { object::id_from_address(@0xABCD) }

fun setup(scenario: &mut Scenario): sui::clock::Clock {
    let clock = th::init_protocol(scenario);
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    clock
}

fun create_call(scenario: &mut Scenario, clock: &sui::clock::Clock) {
    ts::next_tx(scenario, th::seller_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    rfq::create_call_auction<BTC, USDC, CALL>(
        &b,
        coin::mint_for_testing<BTC>(AMOUNT, scenario.ctx()),
        RESERVE,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        th::seller_addr(),
        th::seller_addr(),
        origin_id(),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
}

fun bid_call(scenario: &mut Scenario, clock: &sui::clock::Clock, bidder: address, premium: u64) {
    ts::next_tx(scenario, bidder);
    let mut a = ts::take_shared<Auction<BTC, USDC>>(scenario);
    auc::bid<BTC, USDC>(
        &mut a,
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        bidder,
        clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
}

fun assert_coin_value<T>(scenario: &mut Scenario, owner: address, expected: u64) {
    ts::next_tx(scenario, owner);
    let c = ts::take_from_address<Coin<T>>(scenario, owner);
    assert!(c.value() == expected);
    ts::return_to_address(owner, c);
}

// --- create validations ---

#[test]
#[expected_failure(abort_code = 3, location = options_rfq::rfq)] // rfq_too_close_to_expiry
fun test_create_too_close_to_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    // Land inside the window where duration+extension+buffer overruns expiry.
    clock.increment_for_testing(EXPIRY_MS - DURATION_MS - MAX_EXTENSION_MS - 500_000);
    create_call(&mut scenario, &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 8, location = options_rfq::rfq)] // bucket_expired
fun test_create_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    clock.increment_for_testing(EXPIRY_MS);
    create_call(&mut scenario, &clock);
    abort 0
}

// --- settle_call ---

#[test]
fun test_settle_call_winner_full_routing() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_call(&mut scenario, &clock);
    bid_call(&mut scenario, &clock, th::bidder_a(), 150_000);
    clock.increment_for_testing(DURATION_MS + MAX_EXTENSION_MS);

    ts::next_tx(&mut scenario, th::bidder_b()); // permissionless crank
    let meta = ts::take_shared<CallRfq<BTC, USDC, CALL>>(&scenario);
    let a = ts::take_shared<Auction<BTC, USDC>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle_call(meta, a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    // Winner got the option coins; seller got position + net premium
    // (default fee is 0 ⇒ net == gross).
    assert_coin_value<CALL>(&mut scenario, th::bidder_a(), AMOUNT);
    assert_coin_value<USDC>(&mut scenario, th::seller_addr(), 150_000);
    ts::next_tx(&mut scenario, th::seller_addr());
    let pos = ts::take_from_address<Position>(&scenario, th::seller_addr());
    assert!(position::range_start(&pos) == 0);
    assert!(position::range_end(&pos) == (AMOUNT as u128));
    ts::return_to_address(th::seller_addr(), pos);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun test_settle_call_no_winner_refunds() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_call(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    let meta = ts::take_shared<CallRfq<BTC, USDC, CALL>>(&scenario);
    let a = ts::take_shared<Auction<BTC, USDC>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle_call(meta, a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    assert_coin_value<BTC>(&mut scenario, th::seller_addr(), AMOUNT);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 1, location = options_rfq::rfq)] // rfq_auction_mismatch
fun test_settle_call_wrong_auction_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_call(&mut scenario, &clock);
    // A second, unrelated call RFQ on the same bucket.
    create_call(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    // Deliberately pair the FIRST metadata object with the SECOND auction.
    let meta = ts::take_shared<CallRfq<BTC, USDC, CALL>>(&scenario);
    let mut a = ts::take_shared<Auction<BTC, USDC>>(&scenario);
    if (rfq::call_auction_id(&meta) == object::id(&a)) {
        let a2 = ts::take_shared<Auction<BTC, USDC>>(&scenario);
        ts::return_shared(a);
        a = a2;
    };
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle_call(meta, a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    abort 0
}

#[test]
fun test_settle_call_expired_refunds_both() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_call(&mut scenario, &clock);
    bid_call(&mut scenario, &clock, th::bidder_a(), 150_000);
    // Bucket expires mid-auction; recovery needs no deadline.
    clock.increment_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    let meta = ts::take_shared<CallRfq<BTC, USDC, CALL>>(&scenario);
    let a = ts::take_shared<Auction<BTC, USDC>>(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    rfq::settle_call_expired(meta, a, &b, &clock, scenario.ctx());
    ts::return_shared(b);

    assert_coin_value<USDC>(&mut scenario, th::bidder_a(), 150_000);
    assert_coin_value<BTC>(&mut scenario, th::seller_addr(), AMOUNT);
    clock.destroy_for_testing();
    scenario.end();
}

// --- puts ---

fun create_put(scenario: &mut Scenario, clock: &sui::clock::Clock, collateral: u64) {
    ts::next_tx(scenario, th::seller_addr());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(scenario);
    rfq::create_put_auction<BTC, USDC, PUT>(
        &b,
        coin::mint_for_testing<USDC>(collateral, scenario.ctx()),
        AMOUNT,
        RESERVE,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        th::seller_addr(),
        th::seller_addr(),
        origin_id(),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
}

// collateral = ceil(AMOUNT × STRIKE / 10^0) = 10 × 50_000
const PUT_COLLATERAL: u64 = 500_000;

#[test]
#[expected_failure(abort_code = 59, location = options_rfq::rfq)] // put_collateral_mismatch
fun test_create_put_wrong_collateral_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    create_put(&mut scenario, &clock, PUT_COLLATERAL - 1);
    abort 0
}

#[test]
fun test_settle_put_winner_full_routing() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    create_put(&mut scenario, &clock, PUT_COLLATERAL);

    ts::next_tx(&mut scenario, th::bidder_a());
    let mut a = ts::take_shared<Auction<USDC, USDC>>(&scenario);
    auc::bid<USDC, USDC>(
        &mut a,
        coin::mint_for_testing<USDC>(150_000, scenario.ctx()),
        th::bidder_a(),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
    clock.increment_for_testing(DURATION_MS + MAX_EXTENSION_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    let meta = ts::take_shared<PutRfq<BTC, USDC, PUT>>(&scenario);
    let a = ts::take_shared<Auction<USDC, USDC>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle_put(meta, a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    assert_coin_value<PUT>(&mut scenario, th::bidder_a(), AMOUNT);
    assert_coin_value<USDC>(&mut scenario, th::seller_addr(), 150_000);
    ts::next_tx(&mut scenario, th::seller_addr());
    let pos = ts::take_from_address<Position>(&scenario, th::seller_addr());
    assert!(position::range_end(&pos) == (AMOUNT as u128));
    ts::return_to_address(th::seller_addr(), pos);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun test_settle_put_no_winner_refunds_collateral() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    create_put(&mut scenario, &clock, PUT_COLLATERAL);
    clock.increment_for_testing(DURATION_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    let meta = ts::take_shared<PutRfq<BTC, USDC, PUT>>(&scenario);
    let a = ts::take_shared<Auction<USDC, USDC>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq::settle_put(meta, a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    assert_coin_value<USDC>(&mut scenario, th::seller_addr(), PUT_COLLATERAL);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun test_settle_put_expired_refunds_both() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    create_put(&mut scenario, &clock, PUT_COLLATERAL);

    ts::next_tx(&mut scenario, th::bidder_a());
    let mut a = ts::take_shared<Auction<USDC, USDC>>(&scenario);
    auc::bid<USDC, USDC>(
        &mut a,
        coin::mint_for_testing<USDC>(150_000, scenario.ctx()),
        th::bidder_a(),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(a);
    clock.increment_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::bidder_b());
    let meta = ts::take_shared<PutRfq<BTC, USDC, PUT>>(&scenario);
    let a = ts::take_shared<Auction<USDC, USDC>>(&scenario);
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    rfq::settle_put_expired(meta, a, &b, &clock, scenario.ctx());
    ts::return_shared(b);

    assert_coin_value<USDC>(&mut scenario, th::bidder_a(), 150_000);
    assert_coin_value<USDC>(&mut scenario, th::seller_addr(), PUT_COLLATERAL);
    clock.destroy_for_testing();
    scenario.end();
}
