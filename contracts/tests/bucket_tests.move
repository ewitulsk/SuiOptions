#[test_only]
module options_protocol::bucket_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::account;
use options_protocol::admin;
use options_protocol::bucket::{Self, Bucket};
use options_protocol::call_option::{Self, CallOption};
use options_protocol::position::{Self, PositionNFT};
use options_protocol::quote;
use options_protocol::test_helpers::{Self as th, BTC, USDC};

const STRIKE: u128 = 50_000;
const STRIKE_INTERVAL: u128 = 1_000;
const STRIKE_SCALE: u8 = 0; // pre-SO-55 semantics — strike == settlement-smallest per underlying-smallest
const EXPIRY_MS: u64 = 1_000_000;

fun setup_bucket(scenario: &mut Scenario) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    bucket::new_call_option<BTC, USDC>(
        &cap, EXPIRY_MS, STRIKE, STRIKE_INTERVAL, 1, STRIKE_SCALE, scenario.ctx(),
    );
    th::return_admin_cap(scenario, cap);
}

fun setup_three_buckets(scenario: &mut Scenario) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    bucket::new_call_option<BTC, USDC>(
        &cap, EXPIRY_MS, STRIKE, STRIKE_INTERVAL, 3, STRIKE_SCALE, scenario.ctx(),
    );
    th::return_admin_cap(scenario, cap);
}

fun fund_account<T>(scenario: &mut Scenario, owner: address, amount: u64) {
    ts::next_tx(scenario, owner);
    let mut acc = th::take_account(scenario);
    let c = coin::mint_for_testing<T>(amount, scenario.ctx());
    account::deposit(&mut acc, c);
    ts::return_shared(acc);
}

// --- new_call_option ---

#[test]
fun test_new_call_option_creates_count_buckets_at_correct_strikes() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_three_buckets(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    // Take all three shared buckets in sequence and collect strikes.
    let mut strikes: vector<u128> = vector[];
    let b1 = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    strikes.push_back(bucket::strike(&b1));
    ts::return_shared(b1);
    let b2 = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    strikes.push_back(bucket::strike(&b2));
    ts::return_shared(b2);
    let b3 = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    strikes.push_back(bucket::strike(&b3));
    ts::return_shared(b3);

    // The order isn't guaranteed; sort and verify the set.
    let mut seen_lo = false;
    let mut seen_mid = false;
    let mut seen_hi = false;
    let mut i = 0;
    while (i < strikes.length()) {
        let s = strikes[i];
        if (s == STRIKE) seen_lo = true;
        if (s == STRIKE + STRIKE_INTERVAL) seen_mid = true;
        if (s == STRIKE + 2 * STRIKE_INTERVAL) seen_hi = true;
        i = i + 1;
    };
    assert!(seen_lo && seen_mid && seen_hi, 0);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 22, location = options_protocol::bucket)] // count_must_be_positive
fun test_new_call_option_zero_count_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    bucket::new_call_option<BTC, USDC>(
        &cap, EXPIRY_MS, STRIKE, STRIKE_INTERVAL, 0, STRIKE_SCALE, scenario.ctx(),
    );
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- strike_scale + round-half-up math ---

#[test]
fun test_pow10_table() {
    assert!(bucket::pow10_for_testing(0) == 1, 0);
    assert!(bucket::pow10_for_testing(1) == 10, 0);
    assert!(bucket::pow10_for_testing(2) == 100, 0);
    assert!(bucket::pow10_for_testing(9) == 1_000_000_000, 0);
    // Boundary: 10^38 still fits in u128 (u128::MAX ≈ 3.4e38). 39 would
    // overflow the loop's multiply — see test_pow10_above_max_aborts.
    assert!(
        bucket::pow10_for_testing(38) == 100_000_000_000_000_000_000_000_000_000_000_000_000,
        0,
    );
}

#[test]
#[expected_failure(abort_code = 25, location = options_protocol::bucket)] // strike_scale_too_large
fun test_pow10_above_max_aborts() {
    // MAX_STRIKE_SCALE=38; 39 trips the assert before the loop's u128
    // multiply could overflow on its own.
    let _ = bucket::pow10_for_testing(39);
}

#[test]
fun test_apply_strike_scale_zero_is_plain_multiply() {
    // scale=0 → identity vs old behavior. 100 × 50_000 = 5_000_000.
    assert!(bucket::apply_strike_for_testing(100, 50_000, 0) == 5_000_000, 0);
}

#[test]
fun test_apply_strike_round_half_up_boundaries() {
    // scale=1, divisor=10, half=5.
    //   amount=1 × strike=4 = 4   → (4 + 5)/10 = 0  (0.4 → 0)
    //   amount=1 × strike=5 = 5   → (5 + 5)/10 = 1  (0.5 → 1, half rounds UP)
    //   amount=1 × strike=6 = 6   → (6 + 5)/10 = 1  (0.6 → 1)
    //   amount=1 × strike=14 = 14 → (14 + 5)/10 = 1 (1.4 → 1)
    //   amount=1 × strike=15 = 15 → (15 + 5)/10 = 2 (1.5 → 2)
    assert!(bucket::apply_strike_for_testing(1, 4,  1) == 0, 0);
    assert!(bucket::apply_strike_for_testing(1, 5,  1) == 1, 0);
    assert!(bucket::apply_strike_for_testing(1, 6,  1) == 1, 0);
    assert!(bucket::apply_strike_for_testing(1, 14, 1) == 1, 0);
    assert!(bucket::apply_strike_for_testing(1, 15, 1) == 2, 0);
}

#[test]
fun test_apply_strike_tdeep_at_15_cents() {
    // TDEEP/TUSDC at $0.15, scheduler picks scale=5.
    //   spot_chain_scaled = 0.15 × 10^5 = 15_000 (strike representing $0.15).
    //   strike_scale = 5 → divisor 100_000.
    //   Exercise 1 TDEEP-smallest:  1 × 15_000 / 100_000 → (15000 + 50000)/100000 = 0
    //     (dust loss in buyer's favor; matches what round-half-up gives at 0.15)
    //   Exercise 10 TDEEP-smallest: 10 × 15_000 / 100_000 → (150000 + 50000)/100000 = 2
    //     (round_half_up(1.5) = 2)
    //   Exercise 100 TDEEP-smallest: 100 × 15_000 / 100_000 = 15  (exact)
    //   Exercise 1_000_000 TDEEP-smallest (1 TDEEP): 150_000 settlement-smallest = $0.15. ✓
    assert!(bucket::apply_strike_for_testing(1, 15_000, 5) == 0, 0);
    assert!(bucket::apply_strike_for_testing(10, 15_000, 5) == 2, 0);
    assert!(bucket::apply_strike_for_testing(100, 15_000, 5) == 15, 0);
    assert!(bucket::apply_strike_for_testing(1_000_000, 15_000, 5) == 150_000, 0);
}

#[test]
fun test_new_call_option_records_strike_scale() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    bucket::new_call_option<BTC, USDC>(
        &cap, EXPIRY_MS, 12_345, 100, 2, /*strike_scale*/ 4, scenario.ctx(),
    );
    th::return_admin_cap(&scenario, cap);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b1 = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    assert!(bucket::strike_scale(&b1) == 4, 0);
    ts::return_shared(b1);
    let b2 = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    assert!(bucket::strike_scale(&b2) == 4, 0);
    ts::return_shared(b2);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 25, location = options_protocol::bucket)] // strike_scale_too_large
fun test_new_call_option_scale_above_max_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    bucket::new_call_option<BTC, USDC>(
        &cap, EXPIRY_MS, STRIKE, STRIKE_INTERVAL, 1, /*strike_scale*/ 39, scenario.ctx(),
    );
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Writer flow ---

#[test]
fun test_writer_flow_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let write_amount: u64 = 100;
    let premium: u64 = 5_000_000;

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let underlying = coin::mint_for_testing<BTC>(write_amount, scenario.ctx());
    let zero_settlement = coin::zero<USDC>(scenario.ctx());

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        underlying,
        zero_settlement,
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        &clock,
        scenario.ctx(),
    );

    let bucket_id = object::id(&b);
    assert!(bucket::total_written(&b) == (write_amount as u128), 0);
    assert!(bucket::underlying_balance(&b) == write_amount, 0);
    assert!(bucket::exercise_cursor(&b) == 0, 0);
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - premium, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // Writer receives net premium and position NFT.
    ts::next_tx(&mut scenario, th::writer_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == premium, 0);
    coin::burn_for_testing(net);
    let pos = ts::take_from_sender<PositionNFT>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    assert!(position::bucket_id(&pos) == bucket_id, 0);
    ts::return_to_sender(&scenario, pos);

    // Trader MM receives call option.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    assert!(call_option::amount(&call) == write_amount, 0);
    assert!(call_option::bucket_id(&call) == bucket_id, 0);
    ts::return_to_sender(&scenario, call);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_writer_flow_with_fee_skim() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    // Set fee to 50 bps.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::set_fee_bps(&cap, &mut config, 50);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(config);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let write_amount: u64 = 100;
    let premium: u64 = 1_000_000;
    let expected_fee = 5_000; // 50 bps of 1_000_000
    let expected_net = 995_000;

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(write_amount, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        &clock,
        scenario.ctx(),
    );

    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - premium, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    ts::next_tx(&mut scenario, th::writer_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == expected_net, 0);
    coin::burn_for_testing(net);

    ts::next_tx(&mut scenario, th::admin_addr());
    let treasury_after = th::take_treasury(&scenario);
    assert!(options_protocol::treasury::balance_of<USDC>(&treasury_after) == expected_fee, 0);
    ts::return_shared(treasury_after);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Trader flow ---

#[test]
fun test_trader_flow_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::writer_mm_addr(), th::pubkey_a());
    fund_account<BTC>(&mut scenario, th::writer_mm_addr(), 1_000);

    ts::next_tx(&mut scenario, th::trader_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let write_amount: u64 = 80;
    let premium: u64 = 7_500_000;

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::writer_mm_addr(),    // signer (writer MM) gets the position NFT
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::zero<BTC>(scenario.ctx()),
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        bucket::trader_flow(),
        th::writer_mm_addr(),    // position NFT recipient = MM
        th::trader_addr(),       // call token recipient = retail trader
        sq,
        &clock,
        scenario.ctx(),
    );

    let bucket_id = object::id(&b);
    assert!(bucket::total_written(&b) == (write_amount as u128), 0);
    assert!(bucket::underlying_balance(&b) == write_amount, 0);
    assert!(account::balance_of<BTC>(&mm_acc) == 1_000 - write_amount, 0);
    assert!(account::balance_of<USDC>(&mm_acc) == premium, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // Trader gets the call option.
    ts::next_tx(&mut scenario, th::trader_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    assert!(call_option::amount(&call) == write_amount, 0);
    assert!(call_option::bucket_id(&call) == bucket_id, 0);
    ts::return_to_sender(&scenario, call);

    // Writer MM gets the position NFT.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    let pos = ts::take_from_sender<PositionNFT>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    ts::return_to_sender(&scenario, pos);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- execute_write rejection cases ---

#[test]
#[expected_failure(abort_code = 8, location = options_protocol::bucket)] // bucket_expired
fun test_execute_write_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    clock.set_for_testing(EXPIRY_MS); // at expiry: now >= expiry → reject

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        50,
        1_000,
        EXPIRY_MS + 10_000,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 5, location = options_protocol::bucket)] // quote_bucket_mismatch
fun test_execute_write_bucket_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id_from_address(@0xDEAD), // wrong bucket
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 12, location = options_protocol::bucket)] // amount_mismatch — underlying != write_amount
fun test_writer_flow_amount_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(49, scenario.ctx()), // off-by-one
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 7, location = options_protocol::bucket)] // quote_recipient_mismatch
fun test_writer_flow_recipient_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),     // signer expects this address as call token recipient
        object::id(&b),
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::stranger_addr(),      // mismatched call token recipient
        sq,
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Exercise ---

fun write_via_helper(
    scenario: &mut Scenario,
    clock: &sui::clock::Clock,
    amount: u64,
    premium: u64,
    nonce: u64,
) {
    ts::next_tx(scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(scenario);
    let config = th::take_config(scenario);
    let mut treasury = th::take_treasury(scenario);
    let mut mm_acc = th::take_account(scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        amount,
        premium,
        EXPIRY_MS,
        nonce,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    bucket::execute_write_for_testing<BTC, USDC>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::mint_for_testing<BTC>(amount, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq,
        clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);
}

#[test]
fun test_exercise_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 100, 5_000_000, 1);

    // Trader MM holds the call option; have them exercise 40 of it.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut call = ts::take_from_sender<CallOption>(&scenario);
    let exercise_chunk = call_option::split(&mut call, 40, scenario.ctx());
    ts::return_to_sender(&scenario, call);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let settlement_payment = coin::mint_for_testing<USDC>((((40 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC>(
        &mut b,
        exercise_chunk,
        settlement_payment,
        &clock,
        scenario.ctx(),
    );
    assert!(underlying.value() == 40, 0);
    assert!(bucket::exercise_cursor(&b) == 40, 0);
    assert!(bucket::underlying_balance(&b) == 60, 0);
    assert!(bucket::settlement_balance(&b) == (((40 as u128) * STRIKE) as u64), 0);

    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 13, location = options_protocol::bucket)] // settlement_amount_mismatch
fun test_exercise_settlement_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let underpaid = coin::mint_for_testing<USDC>((((50 as u128) * STRIKE) as u64) - 1, scenario.ctx()); // one short
    let underlying = bucket::exercise<BTC, USDC>(
        &mut b,
        call,
        underpaid,
        &clock,
        scenario.ctx(),
    );
    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_protocol::bucket)] // bucket_expired
fun test_exercise_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((50 as u128) * STRIKE) as u64), scenario.ctx());
    let u = bucket::exercise<BTC, USDC>(&mut b, call, payment, &clock, scenario.ctx());
    coin::burn_for_testing(u);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 17, location = options_protocol::bucket)] // call_option_bucket_mismatch
fun test_exercise_call_from_other_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    // Mint a call option pointing to a different bucket.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let bogus_call = call_option::mint(object::id_from_address(@0xDEAD), 10, scenario.ctx());

    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((10 as u128) * STRIKE) as u64), scenario.ctx());
    let u = bucket::exercise<BTC, USDC>(&mut b, bogus_call, payment, &clock, scenario.ctx());
    coin::burn_for_testing(u);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Redeem ---

#[test]
#[expected_failure(abort_code = 9, location = options_protocol::bucket)] // bucket_not_expired
fun test_redeem_before_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<PositionNFT>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC>(&mut b, pos, &clock, scenario.ctx());
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_fully_unexercised_returns_all_underlying() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 80, 1_000, 1);

    // No exercises occur. Advance past expiry and redeem.
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<PositionNFT>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 80, 0);
    assert!(s.value() == 0, 0);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_fully_exercised_returns_all_settlement() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 60, 1_000, 1);

    // Exercise the entire amount.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((60 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC>(&mut b, call, payment, &clock, scenario.ctx());
    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<PositionNFT>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 0, 0);
    assert!(s.value() == (((60 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_fifo_assignment_two_writers_partial_exercise() {
    // Two consecutive writes: writer (range [0,100)) then writer (range [100,150)).
    // Exercise 120 → first writer fully exercised, second writer 20/50 exercised.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 100_000_000);

    write_via_helper(&mut scenario, &clock, 100, 1_000, 1);

    // Capture the first position NFT before second write so it stays with writer_addr.
    // Both writes deliver to writer_addr() in the helper; we'll track the position by range_end.
    write_via_helper(&mut scenario, &clock, 50, 1_000, 2);

    // Now writer holds two PositionNFTs. Exercise 120 via call options held by trader MM.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut call_a = ts::take_from_sender<CallOption>(&scenario);
    // Trader MM should have received two call options; combine them.
    let call_b = ts::take_from_sender<CallOption>(&scenario);
    call_option::join(&mut call_a, call_b);
    assert!(call_option::amount(&call_a) == 150, 0);

    let exercise_piece = call_option::split(&mut call_a, 120, scenario.ctx());
    ts::return_to_sender(&scenario, call_a);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((120 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC>(&mut b, exercise_piece, payment, &clock, scenario.ctx());
    coin::burn_for_testing(underlying);
    assert!(bucket::exercise_cursor(&b) == 120, 0);
    ts::return_shared(b);

    // Now expire and redeem both positions.
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos_a = ts::take_from_sender<PositionNFT>(&scenario);
    let pos_b = ts::take_from_sender<PositionNFT>(&scenario);

    // Identify each by range_end.
    let (early, late) = if (position::range_end(&pos_a) == 100) { (pos_a, pos_b) } else { (pos_b, pos_a) };

    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);

    let (u_early, s_early) = bucket::redeem_position<BTC, USDC>(&mut b, early, &clock, scenario.ctx());
    assert!(u_early.value() == 0, 0);
    assert!(s_early.value() == (((100 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(u_early);
    coin::burn_for_testing(s_early);

    let (u_late, s_late) = bucket::redeem_position<BTC, USDC>(&mut b, late, &clock, scenario.ctx());
    assert!(u_late.value() == 30, 0);            // 50 written - 20 exercised
    assert!(s_late.value() == (((20 as u128) * STRIKE) as u64), 0);   // 20 exercised
    coin::burn_for_testing(u_late);
    coin::burn_for_testing(s_late);

    ts::return_shared(b);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- burn_expired_option ---

#[test]
fun test_burn_expired_option_after_expiry() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 30, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    bucket::burn_expired_option<BTC, USDC>(&mut b, call, &clock, scenario.ctx());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 9, location = options_protocol::bucket)] // bucket_not_expired
fun test_burn_expired_option_before_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 30, 1_000, 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<CallOption>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    bucket::burn_expired_option<BTC, USDC>(&mut b, call, &clock, scenario.ctx());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- cleanup_bucket ---

#[test]
fun test_cleanup_bucket_when_drained() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    bucket::cleanup_bucket<BTC, USDC>(&cap, b, &clock);
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 10, location = options_protocol::bucket)] // bucket_not_drained
fun test_cleanup_bucket_with_remaining_balance_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_via_helper(&mut scenario, &clock, 10, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC>>(&scenario);
    bucket::cleanup_bucket<BTC, USDC>(&cap, b, &clock);
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}
