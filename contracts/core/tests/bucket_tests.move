#[test_only]
module options_core::bucket_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin;
use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::quote;
use options_core::test_helpers::{Self as th, BTC, USDC, CALL, CALL2, CALL3};

const STRIKE: u128 = 50_000;
const STRIKE_INTERVAL: u128 = 1_000;
const STRIKE_SCALE: u8 = 0; // pre-SO-55 semantics — strike == settlement-smallest per underlying-smallest
const EXPIRY_MS: u64 = 1_000_000;

fun setup_bucket(scenario: &mut Scenario) {
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
}

fun setup_three_buckets(scenario: &mut Scenario) {
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::new_bucket<BTC, USDC, CALL2>(scenario, EXPIRY_MS, STRIKE + STRIKE_INTERVAL, STRIKE_SCALE);
    th::new_bucket<BTC, USDC, CALL3>(scenario, EXPIRY_MS, STRIKE + 2 * STRIKE_INTERVAL, STRIKE_SCALE);
}

// --- create_bucket ---

#[test]
fun test_create_bucket_sets_correct_strikes() {
    // Each bucket now carries a distinct option-coin type; the off-chain
    // scheduler fans a strike grid out into one create_bucket per strike.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_three_buckets(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b1 = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(bucket::strike(&b1) == STRIKE, 0);
    assert!(bucket::call_supply(&b1) == 0, 0);
    ts::return_shared(b1);
    let b2 = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    assert!(bucket::strike(&b2) == STRIKE + STRIKE_INTERVAL, 0);
    ts::return_shared(b2);
    let b3 = ts::take_shared<Bucket<BTC, USDC, CALL3>>(&scenario);
    assert!(bucket::strike(&b3) == STRIKE + 2 * STRIKE_INTERVAL, 0);
    ts::return_shared(b3);

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
#[expected_failure(abort_code = 25, location = options_core::bucket)] // strike_scale_too_large
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
fun test_apply_strike_sub_dollar_same_decimals() {
    // SO-55 regression: a sub-dollar asset against a same-decimals stablecoin
    // is the case where a plain integer ratio loses all resolution. TMICRO is
    // a 6-decimal stand-in for that shape, not a real token.
    // TMICRO/TUSDC at $0.15, scheduler picks scale=5.
    //   spot_chain_scaled = 0.15 × 10^5 = 15_000 (strike representing $0.15).
    //   strike_scale = 5 → divisor 100_000.
    //   Exercise 1 TMICRO-smallest:  1 × 15_000 / 100_000 → (15000 + 50000)/100000 = 0
    //     (dust loss in buyer's favor; matches what round-half-up gives at 0.15)
    //   Exercise 10 TMICRO-smallest: 10 × 15_000 / 100_000 → (150000 + 50000)/100000 = 2
    //     (round_half_up(1.5) = 2)
    //   Exercise 100 TMICRO-smallest: 100 × 15_000 / 100_000 = 15  (exact)
    //   Exercise 1_000_000 TMICRO-smallest (1 TMICRO): 150_000 settlement-smallest = $0.15. ✓
    assert!(bucket::apply_strike_for_testing(1, 15_000, 5) == 0, 0);
    assert!(bucket::apply_strike_for_testing(10, 15_000, 5) == 2, 0);
    assert!(bucket::apply_strike_for_testing(100, 15_000, 5) == 15, 0);
    assert!(bucket::apply_strike_for_testing(1_000_000, 15_000, 5) == 150_000, 0);
}

#[test]
fun test_create_bucket_records_strike_scale() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, 12_345, /*strike_scale*/ 4);
    th::new_bucket<BTC, USDC, CALL2>(&mut scenario, EXPIRY_MS, 12_445, /*strike_scale*/ 4);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b1 = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(bucket::strike_scale(&b1) == 4, 0);
    ts::return_shared(b1);
    let b2 = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    assert!(bucket::strike_scale(&b2) == 4, 0);
    ts::return_shared(b2);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 25, location = options_core::bucket)] // strike_scale_too_large
fun test_create_bucket_scale_above_max_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, /*strike_scale*/ 39);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Writer flow ---

#[test]
fun test_writer_flow_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let write_amount: u64 = 100;
    let premium: u64 = 5_000_000;

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    // The MM's released premium (Balance stand-in) + the writer's underlying.
    let premium_funds = coin::mint_for_testing<USDC>(premium, scenario.ctx()).into_balance();
    let underlying = coin::mint_for_testing<BTC>(write_amount, scenario.ctx());

    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        premium_funds,
        underlying,
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    let bucket_id = object::id(&b);
    assert!(bucket::total_written(&b) == (write_amount as u128), 0);
    assert!(bucket::underlying_balance(&b) == write_amount, 0);
    assert!(bucket::exercise_cursor(&b) == 0, 0);
    assert!(bucket::call_supply(&b) == write_amount, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);

    // Writer receives net premium and position Object.
    ts::next_tx(&mut scenario, th::writer_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == premium, 0);
    coin::burn_for_testing(net);
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    assert!(position::bucket_id(&pos) == bucket_id, 0);
    ts::return_to_sender(&scenario, pos);

    // Trader MM receives the option as a fungible Coin<CALL> at the quote's
    // signer_token_recipient.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    assert!(call.value() == write_amount, 0);
    ts::return_to_sender(&scenario, call);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_writer_flow_with_fee_skim() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    // Set fee to 50 bps.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::set_fee_bps(&cap, &mut config, 50);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(config);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let write_amount: u64 = 100;
    let premium: u64 = 1_000_000;
    let expected_fee = 5_000; // 50 bps of 1_000_000
    let expected_net = 995_000;

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );

    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(premium, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(write_amount, scenario.ctx()),
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);

    ts::next_tx(&mut scenario, th::writer_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == expected_net, 0);
    coin::burn_for_testing(net);

    ts::next_tx(&mut scenario, th::admin_addr());
    let treasury_after = th::take_treasury(&scenario);
    assert!(options_core::treasury::balance_of<USDC>(&treasury_after) == expected_fee, 0);
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
    th::create_signer(&mut scenario, th::writer_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::trader_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let write_amount: u64 = 80;
    let premium: u64 = 7_500_000;

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::writer_mm_addr(),    // signer (writer MM) gets the position Object + net premium
        &b,
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_trader_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );

    bucket::execute_trader_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        // The MM's released underlying write collateral (Balance stand-in).
        coin::mint_for_testing<BTC>(write_amount, scenario.ctx()).into_balance(),
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        th::trader_addr(),       // call token recipient = retail trader
        &clock,
        scenario.ctx(),
    );

    assert!(bucket::total_written(&b) == (write_amount as u128), 0);
    assert!(bucket::underlying_balance(&b) == write_amount, 0);
    assert!(bucket::call_supply(&b) == write_amount, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);

    // Trader gets the call option coin.
    ts::next_tx(&mut scenario, th::trader_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    assert!(call.value() == write_amount, 0);
    ts::return_to_sender(&scenario, call);

    // Writer MM gets the position Object + the net premium coin.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == premium, 0);
    coin::burn_for_testing(net);
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    ts::return_to_sender(&scenario, pos);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- request/execute rejection cases ---

#[test]
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_execute_write_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    clock.set_for_testing(EXPIRY_MS); // at expiry: now >= expiry → reject

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        50,
        1_000,
        EXPIRY_MS + 10_000,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 5, location = options_core::bucket)] // quote_spec_mismatch
fun test_execute_write_spec_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Right pair and expiry, wrong strike: the spec no longer describes this
    // bucket, so the quote cannot be spent against it.
    let q = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        ((STRIKE as u64) + 1), // wrong strike
        STRIKE_SCALE,
        /* is_put */ false,
        std::u128::max_value!(),
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 12, location = options_core::bucket)] // amount_mismatch — underlying != write_amount
fun test_writer_flow_amount_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(49, scenario.ctx()), // off-by-one
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 60, location = options_core::bucket)] // request_flow_mismatch
fun test_writer_request_into_trader_execute_aborts() {
    // Same-type bucket (Underlying == Settlement) so the premium-sized
    // writer potato typechecks into the trader execute — the flow tag must
    // still reject it.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<USDC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<USDC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<USDC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_trader_flow<USDC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()),
        th::trader_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 60, location = options_core::bucket)] // request_flow_mismatch
fun test_trader_request_into_writer_execute_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_bucket<USDC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::create_signer(&mut scenario, th::writer_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::trader_addr());
    let mut b = ts::take_shared<Bucket<USDC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::writer_mm_addr(),
        &b,
        50,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_trader_flow_for_testing<USDC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_writer_flow<USDC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(50, scenario.ctx()).into_balance(),
        coin::mint_for_testing<USDC>(50, scenario.ctx()),
        th::trader_addr(),
        &clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
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
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    let config = th::take_config(scenario);
    let wl = th::take_whitelist(scenario);
    let mut treasury = th::take_treasury(scenario);
    let mut signer = th::take_signer(scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        amount,
        premium,
        EXPIRY_MS,
        nonce,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(premium, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(amount, scenario.ctx()),
        th::writer_addr(),
        clock,
        scenario.ctx(),
    );

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);
}

#[test]
fun test_exercise_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 100, 5_000_000, 1);

    // Trader MM holds the call option; have them exercise 40 of it.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let exercise_chunk = coin::split(&mut call, 40, scenario.ctx());
    ts::return_to_sender(&scenario, call);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let settlement_payment = coin::mint_for_testing<USDC>((((40 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC, CALL>(
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
    // 100 written − 40 burned = 60 outstanding.
    assert!(bucket::call_supply(&b) == 60, 0);

    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 13, location = options_core::bucket)] // settlement_amount_mismatch
fun test_exercise_settlement_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let underpaid = coin::mint_for_testing<USDC>((((50 as u128) * STRIKE) as u64) - 1, scenario.ctx()); // one short
    let underlying = bucket::exercise<BTC, USDC, CALL>(
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
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_exercise_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((50 as u128) * STRIKE) as u64), scenario.ctx());
    let u = bucket::exercise<BTC, USDC, CALL>(&mut b, call, payment, &clock, scenario.ctx());
    coin::burn_for_testing(u);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Redeem ---

#[test]
#[expected_failure(abort_code = 9, location = options_core::bucket)] // bucket_not_expired
fun test_redeem_before_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos, &clock, scenario.ctx());
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
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 80, 1_000, 1);

    // No exercises occur. Advance past expiry and redeem.
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos, &clock, scenario.ctx());
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
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 60, 1_000, 1);

    // Exercise the entire amount.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((60 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC, CALL>(&mut b, call, payment, &clock, scenario.ctx());
    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos, &clock, scenario.ctx());
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
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    write_via_helper(&mut scenario, &clock, 100, 1_000, 1);

    // Capture the first position Object before second write so it stays with writer_addr.
    // Both writes deliver to writer_addr() in the helper; we'll track the position by range_end.
    write_via_helper(&mut scenario, &clock, 50, 1_000, 2);

    // Now writer holds two Positions. Exercise 120 via call options held by trader MM.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut call_a = ts::take_from_sender<Coin<CALL>>(&scenario);
    // Trader MM should have received two call coins; combine them.
    let call_b = ts::take_from_sender<Coin<CALL>>(&scenario);
    coin::join(&mut call_a, call_b);
    assert!(call_a.value() == 150, 0);

    let exercise_piece = coin::split(&mut call_a, 120, scenario.ctx());
    ts::return_to_sender(&scenario, call_a);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((120 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC, CALL>(&mut b, exercise_piece, payment, &clock, scenario.ctx());
    coin::burn_for_testing(underlying);
    assert!(bucket::exercise_cursor(&b) == 120, 0);
    ts::return_shared(b);

    // Now expire and redeem both positions.
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos_a = ts::take_from_sender<Position>(&scenario);
    let pos_b = ts::take_from_sender<Position>(&scenario);

    // Identify each by range_end.
    let (early, late) = if (position::range_end(&pos_a) == 100) { (pos_a, pos_b) } else { (pos_b, pos_a) };

    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);

    let (u_early, s_early) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, early, &clock, scenario.ctx());
    assert!(u_early.value() == 0, 0);
    assert!(s_early.value() == (((100 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(u_early);
    coin::burn_for_testing(s_early);

    let (u_late, s_late) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, late, &clock, scenario.ctx());
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
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 30, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::burn_expired_option<BTC, USDC, CALL>(&mut b, call, &clock, scenario.ctx());
    assert!(bucket::call_supply(&b) == 0, 0);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 9, location = options_core::bucket)] // bucket_not_expired
fun test_burn_expired_option_before_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 30, 1_000, 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::burn_expired_option<BTC, USDC, CALL>(&mut b, call, &clock, scenario.ctx());
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
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::cleanup_bucket<BTC, USDC, CALL>(&cap, b, &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 10, location = options_core::bucket)] // bucket_not_drained
fun test_cleanup_bucket_with_remaining_balance_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 10, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    bucket::cleanup_bucket<BTC, USDC, CALL>(&cap, b, &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- invalidate / revalidate ---

fun invalidate(scenario: &mut Scenario, clock: &sui::clock::Clock, reason: vector<u8>) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    bucket::invalidate_bucket<BTC, USDC, CALL>(&cap, &mut b, reason, clock, scenario.ctx());
    th::return_admin_cap(scenario, cap);
    ts::return_shared(b);
}

fun revalidate(scenario: &mut Scenario, clock: &sui::clock::Clock, reason: vector<u8>) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    bucket::revalidate_bucket<BTC, USDC, CALL>(&cap, &mut b, reason, clock, scenario.ctx());
    th::return_admin_cap(scenario, cap);
    ts::return_shared(b);
}

#[test]
fun test_bucket_invalidated_defaults_false_then_set_true() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(!bucket::invalidated(&b), 0);
    ts::return_shared(b);

    invalidate(&mut scenario, &clock, b"wrong strike");

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(bucket::invalidated(&b), 0);
    ts::return_shared(b);

    revalidate(&mut scenario, &clock, b"fixed");

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(!bucket::invalidated(&b), 0);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 26, location = options_core::bucket)] // bucket_invalidated
fun test_execute_write_on_invalidated_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    invalidate(&mut scenario, &clock, b"bad config");

    write_via_helper(&mut scenario, &clock, 50, 1_000, 1);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_revalidate_re_enables_execute_write() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    invalidate(&mut scenario, &clock, b"hold");
    revalidate(&mut scenario, &clock, b"resumed");

    write_via_helper(&mut scenario, &clock, 25, 1_000, 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(bucket::total_written(&b) == 25, 0);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_exercise_still_works_when_invalidated() {
    // Write first, then invalidate, then exercise — the freeze is on new
    // writes only; outstanding call holders can still exercise pre-expiry.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 60, 1_000, 1);

    invalidate(&mut scenario, &clock, b"post-write");

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let call = ts::take_from_sender<Coin<CALL>>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let payment = coin::mint_for_testing<USDC>((((60 as u128) * STRIKE) as u64), scenario.ctx());
    let underlying = bucket::exercise<BTC, USDC, CALL>(&mut b, call, payment, &clock, scenario.ctx());
    assert!(underlying.value() == 60, 0);
    assert!(bucket::exercise_cursor(&b) == 60, 0);
    coin::burn_for_testing(underlying);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_still_works_when_invalidated() {
    // Invalidate pre-expiry, advance past expiry, redeem proceeds normally.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    write_via_helper(&mut scenario, &clock, 40, 1_000, 1);

    invalidate(&mut scenario, &clock, b"recall");
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 40, 0);
    assert!(s.value() == 0, 0);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 26, location = options_core::bucket)] // bucket_invalidated
fun test_double_invalidate_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    invalidate(&mut scenario, &clock, b"first");
    invalidate(&mut scenario, &clock, b"second");

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 27, location = options_core::bucket)] // bucket_not_invalidated
fun test_revalidate_when_not_invalidated_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    revalidate(&mut scenario, &clock, b"nothing to undo");

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_invalidate_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    clock.set_for_testing(EXPIRY_MS);

    invalidate(&mut scenario, &clock, b"too late");

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_revalidate_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    invalidate(&mut scenario, &clock, b"pre-expiry");
    clock.set_for_testing(EXPIRY_MS);
    revalidate(&mut scenario, &clock, b"too late");

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- write_collateralized (venue write core) ---

#[test]
fun test_write_collateralized_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let underlying = coin::mint_for_testing<BTC>(100, scenario.ctx());

    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &wl,
        underlying,
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);

    // Cursor advanced, escrow held, supply == outstanding.
    assert!(bucket::total_written(&b) == 100, 0);
    assert!(bucket::underlying_balance(&b) == 100, 0);
    assert!(bucket::call_supply(&b) == 100, 0);
    assert!(bucket::exercise_cursor(&b) == 0, 0);

    // Returned Position covers [0, 100); returned Coin<Call> is the amount.
    assert!(position::bucket_id(&pos) == object::id(&b), 0);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == 100, 0);
    assert!(call.value() == 100, 0);

    // CollateralizedWrite event contents (same-tx event log).
    let emitted = sui::event::events_by_type<options_core::events::CollateralizedWrite>();
    assert!(emitted.length() == 1, 0);
    let expected = options_core::events::new_collateralized_write_for_testing(
        object::id(&b),
        th::writer_addr(),
        object::id(&pos),
        100,
        0,
        100,
    );
    assert!(emitted[0] == expected, 0);

    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(call, th::writer_addr());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_write_collateralized_self_write_round_trip_conservation() {
    // Self-write → partial exercise by the same party → redeem after expiry.
    // Uses a scaled strike (7.5 settlement per underlying unit) so the
    // apply_strike round-half-up path is on the conservation trail:
    // exercising 3 units pays round(22.5) = 23, and the redeem returns
    // exactly that 23 — chain-unit equality end to end.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_bucket<BTC, USDC, CALL2>(&mut scenario, EXPIRY_MS, 75, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL2>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let (pos, mut call) = bucket::write_collateralized<BTC, USDC, CALL2>(
        &mut b,
        &wl,
        coin::mint_for_testing<BTC>(10, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);

    // Exercise 3 of the 10: required settlement = round_half_up(3 × 7.5) = 23.
    let chunk = coin::split(&mut call, 3, scenario.ctx());
    let payment = coin::mint_for_testing<USDC>(23, scenario.ctx());
    let exercised_underlying = bucket::exercise<BTC, USDC, CALL2>(
        &mut b,
        chunk,
        payment,
        &clock,
        scenario.ctx(),
    );
    assert!(exercised_underlying.value() == 3, 0);
    assert!(bucket::settlement_balance(&b) == 23, 0);

    // Redeem the position after expiry: 7 underlying + the same 23 back.
    clock.set_for_testing(EXPIRY_MS + 1);
    let (u, s) = bucket::redeem_position<BTC, USDC, CALL2>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 7, 0);
    assert!(s.value() == 23, 0);

    // Exact conservation: all 10 underlying and all 23 settlement returned
    // to the self-writer; the bucket is fully drained.
    assert!(exercised_underlying.value() + u.value() == 10, 0);
    assert!(bucket::underlying_balance(&b) == 0, 0);
    assert!(bucket::settlement_balance(&b) == 0, 0);

    coin::burn_for_testing(exercised_underlying);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    coin::burn_for_testing(call);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_core::bucket)] // bucket_expired
fun test_write_collateralized_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    clock.set_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &wl,
        coin::mint_for_testing<BTC>(1, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(call, th::writer_addr());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 26, location = options_core::bucket)] // bucket_invalidated
fun test_write_collateralized_on_invalidated_bucket_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    invalidate(&mut scenario, &clock, b"halt writes");

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &wl,
        coin::mint_for_testing<BTC>(1, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(call, th::writer_addr());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 21, location = options_core::bucket)] // zero_amount
fun test_write_collateralized_zero_amount_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let (pos, call) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &wl,
        coin::zero<BTC>(scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(call, th::writer_addr());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- WriteExecuted event regression (post-modularization) ---

#[test]
fun test_write_executed_event_fields_unchanged_by_refactor() {
    // The signed-quote path must be byte-for-byte identical after the
    // do_write/skim_fee extraction: assert the full WriteExecuted event
    // contents, fee math included (50 bps of 1_000_000 → fee 5_000).
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut config = th::take_config(&scenario);
    admin::set_fee_bps(&cap, &mut config, 50);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(config);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        100,
        1_000_000,
        EXPIRY_MS,
        7,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );

    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(100, scenario.ctx()),
        th::writer_addr(),
        &clock,
        scenario.ctx(),
    );

    let emitted = sui::event::events_by_type<options_core::events::WriteExecuted>();
    assert!(emitted.length() == 1, 0);
    let actual = emitted[0];
    let expected = options_core::events::new_write_executed_for_testing(
        object::id(&b),
        object::id(&signer),           // signer_id
        object::id(&signer),           // collateral_source (test-quote dummy = signer id)
        th::trader_mm_addr(),          // signer_token_recipient
        th::writer_addr(),             // executor (tx sender)
        options_core::events::write_executed_position_id(&actual),
        th::writer_addr(),             // position_recipient
        th::trader_mm_addr(),          // call_token_recipient
        100,                           // write_amount
        1_000_000,                     // gross_premium
        5_000,                         // fee (50 bps)
        995_000,                       // net_premium
        0,                             // range_start
        100,                           // range_end
        7,                             // nonce
    );
    assert!(actual == expected, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);

    // The position_id in the event is the Position the recipient received.
    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(
        object::id(&pos) == options_core::events::write_executed_position_id(&actual),
        0,
    );
    ts::return_to_sender(&scenario, pos);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- Venue interleaving: signed-quote + collateralized into one bucket ---

#[test]
fun test_interleaved_venues_share_cursor_and_redeem_exactly() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    // Write 1 (signed quote): [0, 100), position → writer, call → trader MM.
    write_via_helper(&mut scenario, &clock, 100, 1_000, 1);

    // Write 2 (collateralized): [100, 150), both legs stay with the writer.
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let wl2 = th::take_whitelist(&scenario);
    let (pos2, call2) = bucket::write_collateralized<BTC, USDC, CALL>(
        &mut b,
        &wl2,
        coin::mint_for_testing<BTC>(50, scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(wl2);
    assert!(position::range_start(&pos2) == 100, 0);
    assert!(position::range_end(&pos2) == 150, 0);
    ts::return_shared(b);

    // Write 3 (signed quote): [150, 175), position → trader (distinct inventory).
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let wl = th::take_whitelist(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut signer = th::take_signer(&scenario);
    let q = th::new_test_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        25,
        1_000,
        EXPIRY_MS,
        2,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, sq, &clock,
    );
    bucket::execute_writer_flow<BTC, USDC, CALL>(
        &mut b,
        &config,
        &wl,
        &mut treasury,
        req,
        coin::mint_for_testing<USDC>(1_000, scenario.ctx()).into_balance(),
        coin::mint_for_testing<BTC>(25, scenario.ctx()),
        th::trader_addr(),
        &clock,
        scenario.ctx(),
    );
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(wl);
    ts::return_shared(treasury);
    ts::return_shared(signer);

    // Cursor and supply consistent across all three venues.
    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    assert!(bucket::total_written(&b) == 175, 0);
    assert!(bucket::call_supply(&b) == 175, 0);
    assert!(bucket::underlying_balance(&b) == 175, 0);

    // Exercises consume the FIFO front regardless of venue: trader MM
    // exercises its 100 (covers write 1), the stranger exercises 20 of
    // write 2's 50 (exits stay open to non-members). Cursor ends at 120.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    // The MM holds two call coins (100 from write 1, 25 from write 3);
    // merge and carve out exactly 100 to exercise.
    let mut mm_calls = ts::take_from_sender<Coin<CALL>>(&scenario);
    let other = ts::take_from_sender<Coin<CALL>>(&scenario);
    mm_calls.join(other);
    assert!(mm_calls.value() == 125, 0);
    let exercise_100 = coin::split(&mut mm_calls, 100, scenario.ctx());
    let u1 = bucket::exercise<BTC, USDC, CALL>(
        &mut b,
        exercise_100,
        coin::mint_for_testing<USDC>((((100 as u128) * STRIKE) as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(u1.value() == 100, 0);
    coin::burn_for_testing(u1);
    ts::return_to_sender(&scenario, mm_calls);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut call2 = call2;
    let chunk = coin::split(&mut call2, 20, scenario.ctx());
    let u2 = bucket::exercise<BTC, USDC, CALL>(
        &mut b,
        chunk,
        coin::mint_for_testing<USDC>((((20 as u128) * STRIKE) as u64), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    assert!(u2.value() == 20, 0);
    assert!(bucket::exercise_cursor(&b) == 120, 0);
    coin::burn_for_testing(u2);

    // Past expiry, every position redeems to its exact FIFO share.
    clock.set_for_testing(EXPIRY_MS + 1);

    // Write 1's position [0,100): fully exercised.
    ts::next_tx(&mut scenario, th::writer_addr());
    let pos1 = ts::take_from_sender<Position>(&scenario);
    let (ru, rs) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos1, &clock, scenario.ctx());
    assert!(ru.value() == 0, 0);
    assert!(rs.value() == (((100 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(ru);
    coin::burn_for_testing(rs);

    // Write 2's position [100,150): 20 exercised, 30 back.
    ts::next_tx(&mut scenario, th::stranger_addr());
    let (ru, rs) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos2, &clock, scenario.ctx());
    assert!(ru.value() == 30, 0);
    assert!(rs.value() == (((20 as u128) * STRIKE) as u64), 0);
    coin::burn_for_testing(ru);
    coin::burn_for_testing(rs);

    // Write 3's position [150,175): untouched.
    ts::next_tx(&mut scenario, th::trader_addr());
    let pos3 = ts::take_from_sender<Position>(&scenario);
    let (ru, rs) = bucket::redeem_position<BTC, USDC, CALL>(&mut b, pos3, &clock, scenario.ctx());
    assert!(ru.value() == 25, 0);
    assert!(rs.value() == 0, 0);
    coin::burn_for_testing(ru);
    coin::burn_for_testing(rs);

    // Bucket fully drained; outstanding supply = unexercised, unburned calls.
    assert!(bucket::underlying_balance(&b) == 0, 0);
    assert!(bucket::settlement_balance(&b) == 0, 0);
    assert!(bucket::call_supply(&b) == 55, 0); // 25 (trader MM) + 30 (stranger)

    coin::burn_for_testing(call2);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}
