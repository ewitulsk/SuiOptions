#[test_only]
module options_protocol::rfq_put_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::admin;
use options_protocol::position::{Self, Position};
use options_protocol::put_bucket::{Self, PutBucket};
use options_protocol::rfq_put::{Self, PutRfqAuction};
use options_protocol::test_helpers::{Self as th, BTC, USDC, PUT};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 10_000_000;

const DURATION_MS: u64 = 400_000;
const SNIPE_WINDOW_MS: u64 = 60_000;
const SNIPE_EXTENSION_MS: u64 = 120_000;
const MAX_EXTENSION_MS: u64 = 100_000;
const MIN_INCREMENT_BPS: u64 = 500;

fun setup(scenario: &mut Scenario): sui::clock::Clock {
    let clock = th::init_protocol(scenario);
    th::new_put_bucket<BTC, USDC, PUT>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    clock
}

fun seller(): address { th::writer_addr() }
fun bidder_a(): address { th::trader_mm_addr() }
fun bidder_b(): address { th::writer_mm_addr() }

fun origin_id(): ID { object::id_from_address(@0xABCD) }

/// Seller escrows the cash collateral for a put of `amount` underlying-units.
fun create_auction(scenario: &mut Scenario, clock: &sui::clock::Clock, amount: u64, reserve: u64) {
    ts::next_tx(scenario, seller());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(scenario);
    let collateral = put_bucket::required_collateral(&b, amount);
    rfq_put::create<BTC, USDC, PUT>(
        &b,
        coin::mint_for_testing<USDC>(collateral, scenario.ctx()),
        amount,
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

fun place_bid(scenario: &mut Scenario, clock: &sui::clock::Clock, bidder: address, premium: u64) {
    ts::next_tx(scenario, bidder);
    let mut a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(scenario);
    rfq_put::bid<BTC, USDC, PUT>(
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
    let a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(&scenario);
    assert!(rfq_put::amount(&a) == 100, 0);
    assert!(rfq_put::collateral(&a) == 100 * (STRIKE as u64), 0);
    assert!(rfq_put::reserve_premium(&a) == 1_000, 0);
    assert!(rfq_put::deadline_ms(&a) == DURATION_MS, 0);
    assert!(rfq_put::best_premium(&a) == 0, 0);
    assert!(rfq_put::origin(&a) == origin_id(), 0);
    ts::return_shared(a);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 59, location = options_protocol::rfq_put)] // put_collateral_mismatch
fun test_create_wrong_collateral_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    ts::next_tx(&mut scenario, seller());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    rfq_put::create<BTC, USDC, PUT>(
        &b,
        coin::mint_for_testing<USDC>(100 * (STRIKE as u64) - 1, scenario.ctx()), // one short
        100,
        1_000,
        DURATION_MS,
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
#[expected_failure(abort_code = 34, location = options_protocol::rfq_put)] // rfq_duration_too_short
fun test_create_duration_too_short_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    ts::next_tx(&mut scenario, seller());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let collateral = put_bucket::required_collateral(&b, 100);
    rfq_put::create<BTC, USDC, PUT>(
        &b,
        coin::mint_for_testing<USDC>(collateral, scenario.ctx()),
        100,
        1_000,
        299_999,
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

// --- bid + settle ---

#[test]
fun test_bid_and_settle_writes_put() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);

    // Fee 50 bps.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::set_fee_bps(&cap, &mut config, 50);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(config);

    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);
    place_bid(&mut scenario, &clock, bidder_b(), 2_000);

    clock.set_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq_put::settle<BTC, USDC, PUT>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());

    // Write executed for the full slice; collateral is the bucket's cash.
    assert!(put_bucket::total_written(&b) == 100, 0);
    assert!(put_bucket::put_supply(&b) == 100, 0);
    assert!(put_bucket::settlement_balance(&b) == 100 * (STRIKE as u64), 0);
    ts::return_shared(b);
    ts::return_shared(config);

    // Treasury: 50 bps of 2_000 = 10.
    assert!(options_protocol::treasury::balance_of<USDC>(&treasury) == 10, 0);
    ts::return_shared(treasury);

    // Winner (bidder_b) holds the put coins.
    ts::next_tx(&mut scenario, bidder_b());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    assert!(put.value() == 100, 0);
    ts::return_to_sender(&scenario, put);

    // Seller holds the Position [0,100) and the net premium (2_000 − 10).
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
#[expected_failure(abort_code = 30, location = options_protocol::rfq_put)] // rfq_not_closed
fun test_settle_before_deadline_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 1_000);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq_put::settle<BTC, USDC, PUT>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_no_bid_refunds_collateral() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);

    clock.set_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    rfq_put::settle<BTC, USDC, PUT>(a, &mut b, &config, &mut treasury, &clock, scenario.ctx());

    assert!(put_bucket::total_written(&b) == 0, 0);
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);

    // Cash collateral refunded to the proceeds recipient (seller).
    ts::next_tx(&mut scenario, seller());
    let refund = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(refund.value() == 100 * (STRIKE as u64), 0);
    coin::burn_for_testing(refund);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_settle_expired_refunds_both_escrows() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    create_auction(&mut scenario, &clock, 100, 1_000);
    place_bid(&mut scenario, &clock, bidder_a(), 5_000);

    clock.set_for_testing(EXPIRY_MS);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let a = ts::take_shared<PutRfqAuction<BTC, USDC, PUT>>(&scenario);
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    rfq_put::settle_expired<BTC, USDC, PUT>(a, &b, &clock, scenario.ctx());
    assert!(put_bucket::total_written(&b) == 0, 0);
    ts::return_shared(b);

    // Bid back to the bidder, cash collateral back to the seller.
    ts::next_tx(&mut scenario, bidder_a());
    let bid_refund = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(bid_refund.value() == 5_000, 0);
    coin::burn_for_testing(bid_refund);
    ts::next_tx(&mut scenario, seller());
    let collateral = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(collateral.value() == 100 * (STRIKE as u64), 0);
    coin::burn_for_testing(collateral);

    clock.destroy_for_testing();
    ts::end(scenario);
}
