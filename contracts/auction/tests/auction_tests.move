#[test_only]
module auction::auction_tests;

use sui::clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use auction::auction::{Self as auc, Auction};

/// Test asset legs.
public struct GOLD has drop {}
public struct USD has drop {}

/// A venue witness this test module can mint…
public struct VenueAuth has drop {}

/// …and one it uses to impersonate a foreign venue.
public struct WrongAuth has drop {}

const SELLER: address = @0xA11CE;
const BIDDER_A: address = @0xB0B;
const BIDDER_B: address = @0xCAFE;
const PROCEEDS: address = @0xFEED;
const REFUND: address = @0xF00D;

const AMOUNT: u64 = 1_000_000;
const RESERVE: u64 = 500_000;
const DURATION_MS: u64 = 400_000;
const SNIPE_WINDOW_MS: u64 = 60_000;
const SNIPE_EXTENSION_MS: u64 = 120_000;
const MAX_EXTENSION_MS: u64 = 100_000;
const MIN_INCREMENT_BPS: u64 = 500; // 5% for easy numbers

fun origin_id(): ID { object::id_from_address(@0xABCD) }

fun new_clock(scenario: &mut Scenario): clock::Clock {
    clock::create_for_testing(scenario.ctx())
}

fun create_uncoupled(scenario: &mut Scenario, clock: &clock::Clock) {
    ts::next_tx(scenario, SELLER);
    auc::create<GOLD, USD>(
        coin::mint_for_testing<GOLD>(AMOUNT, scenario.ctx()),
        RESERVE,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        PROCEEDS,
        REFUND,
        origin_id(),
        clock,
        scenario.ctx(),
    );
}

fun create_coupled(scenario: &mut Scenario, clock: &clock::Clock) {
    ts::next_tx(scenario, SELLER);
    auc::create_coupled<GOLD, USD, VenueAuth>(
        VenueAuth {},
        coin::mint_for_testing<GOLD>(AMOUNT, scenario.ctx()).into_balance(),
        RESERVE,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        origin_id(),
        clock,
        scenario.ctx(),
    );
}

fun place_bid(scenario: &mut Scenario, clock: &clock::Clock, bidder: address, amount: u64) {
    ts::next_tx(scenario, bidder);
    let mut a = ts::take_shared<Auction<GOLD, USD>>(scenario);
    auc::bid<GOLD, USD>(
        &mut a,
        coin::mint_for_testing<USD>(amount, scenario.ctx()),
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

// --- create ---

#[test]
#[expected_failure(abort_code = 2, location = auction::auction)]
fun test_create_short_duration_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    auc::create<GOLD, USD>(
        coin::mint_for_testing<GOLD>(AMOUNT, scenario.ctx()),
        RESERVE,
        299_999, // < MIN_DURATION_MS
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        PROCEEDS,
        REFUND,
        origin_id(),
        &clock,
        scenario.ctx(),
    );
    abort 0
}

#[test]
#[expected_failure(abort_code = 1, location = auction::auction)]
fun test_create_zero_escrow_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    auc::create<GOLD, USD>(
        coin::zero<GOLD>(scenario.ctx()),
        RESERVE,
        DURATION_MS,
        SNIPE_WINDOW_MS,
        SNIPE_EXTENSION_MS,
        MAX_EXTENSION_MS,
        MIN_INCREMENT_BPS,
        PROCEEDS,
        REFUND,
        origin_id(),
        &clock,
        scenario.ctx(),
    );
    abort 0
}

// --- bid ---

#[test]
#[expected_failure(abort_code = 5, location = auction::auction)]
fun test_bid_below_reserve_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, RESERVE - 1);
    abort 0
}

#[test]
#[expected_failure(abort_code = 5, location = auction::auction)]
fun test_bid_below_min_increment_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, RESERVE);
    // 5% over 500_000 = 525_000; 524_999 must fail.
    place_bid(&mut scenario, &clock, BIDDER_B, 524_999);
    abort 0
}

#[test]
fun test_outbid_refunds_previous_best() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, RESERVE);
    place_bid(&mut scenario, &clock, BIDDER_B, 525_000);
    // A's escrowed bid came back in full.
    assert_coin_value<USD>(&mut scenario, BIDDER_A, RESERVE);
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    assert!(auc::best_bid(&a) == 525_000);
    assert!(auc::best_bidder(&a) == option::some(BIDDER_B));
    ts::return_shared(a);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 3, location = auction::auction)]
fun test_bid_after_deadline_aborts() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);
    place_bid(&mut scenario, &clock, BIDDER_A, RESERVE);
    abort 0
}

#[test]
fun test_anti_snipe_extends_deadline_capped() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);

    // Land a best bid inside the snipe window: extension applies but is
    // capped at max_deadline (400k + 100k = 500k, not now + 120k).
    clock.increment_for_testing(DURATION_MS - 1_000);
    place_bid(&mut scenario, &clock, BIDDER_A, RESERVE);

    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    assert!(auc::deadline_ms(&a) == DURATION_MS + MAX_EXTENSION_MS);
    ts::return_shared(a);

    // The extension is real: a bid at 450k (past the original deadline)
    // still lands.
    clock.increment_for_testing(51_000);
    place_bid(&mut scenario, &clock, BIDDER_B, 600_000);
    clock.destroy_for_testing();
    scenario.end();
}

// --- settle (uncoupled) ---

#[test]
fun test_settle_routes_escrow_and_proceeds() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, 600_000);
    clock.increment_for_testing(DURATION_MS + MAX_EXTENSION_MS);

    ts::next_tx(&mut scenario, BIDDER_B); // settle is permissionless
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    auc::settle<GOLD, USD>(a, &clock, scenario.ctx());

    assert_coin_value<GOLD>(&mut scenario, BIDDER_A, AMOUNT);
    assert_coin_value<USD>(&mut scenario, PROCEEDS, 600_000);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun test_settle_no_winner_refunds_escrow() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);

    ts::next_tx(&mut scenario, BIDDER_B);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    auc::settle<GOLD, USD>(a, &clock, scenario.ctx());

    assert_coin_value<GOLD>(&mut scenario, REFUND, AMOUNT);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 4, location = auction::auction)]
fun test_settle_before_deadline_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    ts::next_tx(&mut scenario, BIDDER_B);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    auc::settle<GOLD, USD>(a, &clock, scenario.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 7, location = auction::auction)]
fun test_settle_coupled_aborts() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, BIDDER_B);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    auc::settle<GOLD, USD>(a, &clock, scenario.ctx());
    abort 0
}

// --- finalize (coupled) ---

#[test]
fun test_finalize_with_witness_hands_back_balances() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, 700_000);
    clock.increment_for_testing(DURATION_MS + MAX_EXTENSION_MS);

    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (mut winner, escrow, receipt) =
        auc::finalize<GOLD, USD, VenueAuth>(VenueAuth {}, a, &clock);
    assert!(escrow.value() == AMOUNT);
    assert!(auc::receipt_amount(&receipt) == AMOUNT);
    assert!(auc::receipt_origin(&receipt) == origin_id());
    let (bidder, recipient, bid) = auc::unpack_bid(winner.extract());
    winner.destroy_none();
    assert!(bidder == BIDDER_A);
    assert!(recipient == BIDDER_A);
    assert!(bid.value() == 700_000);
    // Absorb like a venue would.
    transfer::public_transfer(
        coin::from_balance(escrow, scenario.ctx()), SELLER);
    transfer::public_transfer(
        coin::from_balance(bid, scenario.ctx()), SELLER);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 6, location = auction::auction)]
fun test_finalize_wrong_witness_aborts() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (_winner, _escrow, _receipt) =
        auc::finalize<GOLD, USD, WrongAuth>(WrongAuth {}, a, &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 6, location = auction::auction)]
fun test_finalize_uncoupled_aborts() {
    let mut scenario = ts::begin(SELLER);
    let mut clock = new_clock(&mut scenario);
    create_uncoupled(&mut scenario, &clock);
    clock.increment_for_testing(DURATION_MS);
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (_winner, _escrow, _receipt) =
        auc::finalize<GOLD, USD, VenueAuth>(VenueAuth {}, a, &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 4, location = auction::auction)]
fun test_finalize_before_deadline_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (_winner, _escrow, _receipt) =
        auc::finalize<GOLD, USD, VenueAuth>(VenueAuth {}, a, &clock);
    abort 0
}

#[test]
fun test_finalize_early_recovers_before_deadline() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    place_bid(&mut scenario, &clock, BIDDER_A, 700_000);

    // Venue-decided early recovery (its own preconditions gate this);
    // the standing bid comes back for the venue to refund.
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (mut winner, escrow, _receipt) =
        auc::finalize_early<GOLD, USD, VenueAuth>(VenueAuth {}, a);
    let (bidder, _recipient, bid) = auc::unpack_bid(winner.extract());
    winner.destroy_none();
    assert!(escrow.value() == AMOUNT);
    assert!(bid.value() == 700_000);
    transfer::public_transfer(coin::from_balance(bid, scenario.ctx()), bidder);
    transfer::public_transfer(
        coin::from_balance(escrow, scenario.ctx()), SELLER);
    assert_coin_value<USD>(&mut scenario, BIDDER_A, 700_000);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 6, location = auction::auction)]
fun test_finalize_early_wrong_witness_aborts() {
    let mut scenario = ts::begin(SELLER);
    let clock = new_clock(&mut scenario);
    create_coupled(&mut scenario, &clock);
    ts::next_tx(&mut scenario, SELLER);
    let a = ts::take_shared<Auction<GOLD, USD>>(&scenario);
    let (_winner, _escrow, _receipt) =
        auc::finalize_early<GOLD, USD, WrongAuth>(WrongAuth {}, a);
    abort 0
}
