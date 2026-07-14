#[test_only]
module options_core::put_bucket_tests;

use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::account;
use options_core::admin;
use options_core::bucket;
use options_core::put_bucket::{Self, PutBucket};
use options_core::position::{Self, Position};
use options_core::quote;
use options_core::test_helpers::{Self as th, BTC, USDC, PUT};

const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;
const EXPIRY_MS: u64 = 1_000_000;

fun setup_bucket(scenario: &mut Scenario) {
    th::new_put_bucket<BTC, USDC, PUT>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
}

fun fund_account<T>(scenario: &mut Scenario, owner: address, amount: u64) {
    ts::next_tx(scenario, owner);
    let mut acc = th::take_account(scenario);
    let c = coin::mint_for_testing<T>(amount, scenario.ctx());
    account::deposit(&mut acc, c);
    ts::return_shared(acc);
}

/// Writer-flow write: the writer (executor, tx sender = writer_addr) posts
/// cash collateral; the trader MM (signer) is the buyer paying premium and
/// receiving the put coins. Mirrors how a retail writer sells a put to an MM.
fun write_put_writer(
    scenario: &mut Scenario,
    clock: &sui::clock::Clock,
    write_amount: u64,
    premium: u64,
    nonce: u64,
) {
    ts::next_tx(scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(scenario);
    let config = th::take_config(scenario);
    let mut treasury = th::take_treasury(scenario);
    let mut mm_acc = th::take_account(scenario);

    let collateral_amount = put_bucket::required_collateral(&b, write_amount);
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(), // signer (buyer) recipient == put token recipient
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        nonce,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    let collateral = coin::mint_for_testing<USDC>(collateral_amount, scenario.ctx());
    let zero_premium = coin::zero<USDC>(scenario.ctx());

    put_bucket::execute_write_for_testing<BTC, USDC, PUT>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        collateral,
        zero_premium,
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

// --- create ---

#[test]
fun test_create_put_bucket_sets_fields() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    assert!(put_bucket::strike(&b) == STRIKE, 0);
    assert!(put_bucket::strike_scale(&b) == STRIKE_SCALE, 0);
    assert!(put_bucket::put_supply(&b) == 0, 0);
    assert!(put_bucket::total_written(&b) == 0, 0);
    assert!(put_bucket::total_redeemed(&b) == 0, 0);
    assert!(put_bucket::required_collateral(&b, 100) == 100 * (STRIKE as u64), 0);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 28, location = options_core::put_bucket)] // treasury_cap_not_fresh
fun test_create_put_bucket_rejects_nonfresh_cap() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut tcap = coin::create_treasury_cap_for_testing<PUT>(scenario.ctx());
    // Pre-mint so the cap is no longer zero-supply.
    let minted = coin::mint(&mut tcap, 1, scenario.ctx());
    put_bucket::create_put_bucket<BTC, USDC, PUT>(
        &cap, tcap, EXPIRY_MS, STRIKE, STRIKE_SCALE, scenario.ctx(),
    );
    coin::burn_for_testing(minted);
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- rounding helpers ---

#[test]
fun test_apply_strike_ceil_floor() {
    // scale=1, divisor=10.
    //   1 × 4  = 0.4 → ceil 1, floor 0
    //   1 × 15 = 1.5 → ceil 2, floor 1
    //   2 × 5  = 1.0 → ceil 1, floor 1 (exact)
    assert!(put_bucket::apply_strike_ceil_for_testing(1, 4, 1) == 1, 0);
    assert!(put_bucket::apply_strike_floor_for_testing(1, 4, 1) == 0, 0);
    assert!(put_bucket::apply_strike_ceil_for_testing(1, 15, 1) == 2, 0);
    assert!(put_bucket::apply_strike_floor_for_testing(1, 15, 1) == 1, 0);
    assert!(put_bucket::apply_strike_ceil_for_testing(2, 5, 1) == 1, 0);
    assert!(put_bucket::apply_strike_floor_for_testing(2, 5, 1) == 1, 0);
    // scale=0 → exact multiply both ways.
    assert!(put_bucket::apply_strike_ceil_for_testing(100, 50_000, 0) == 5_000_000, 0);
    assert!(put_bucket::apply_strike_floor_for_testing(100, 50_000, 0) == 5_000_000, 0);
}

// --- writer flow ---

#[test]
fun test_writer_flow_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    let write_amount: u64 = 100;
    let premium: u64 = 1_000_000;
    let collateral = 100 * (STRIKE as u64); // 5_000_000

    write_put_writer(&mut scenario, &clock, write_amount, premium, 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let bucket_id = object::id(&b);
    assert!(put_bucket::total_written(&b) == (write_amount as u128), 0);
    assert!(put_bucket::settlement_balance(&b) == collateral, 0);
    assert!(put_bucket::underlying_balance(&b) == 0, 0);
    assert!(put_bucket::put_supply(&b) == write_amount, 0);
    ts::return_shared(b);

    // MM (signer/buyer) account debited the premium.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mm_acc = th::take_account(&scenario);
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - premium, 0);
    ts::return_shared(mm_acc);

    // Writer receives net premium + Position.
    ts::next_tx(&mut scenario, th::writer_addr());
    let net = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(net.value() == premium, 0);
    coin::burn_for_testing(net);
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(position::range_start(&pos) == 0, 0);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    assert!(position::bucket_id(&pos) == bucket_id, 0);
    ts::return_to_sender(&scenario, pos);

    // Buyer (trader MM) receives the put coins.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    assert!(put.value() == write_amount, 0);
    ts::return_to_sender(&scenario, put);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 59, location = options_core::put_bucket)] // put_collateral_mismatch
fun test_writer_flow_wrong_collateral_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        100,
        1_000_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    // One short of the required 5_000_000.
    let collateral = coin::mint_for_testing<USDC>(100 * (STRIKE as u64) - 1, scenario.ctx());

    put_bucket::execute_write_for_testing<BTC, USDC, PUT>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        collateral,
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

// --- trader flow ---

#[test]
fun test_trader_flow_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::writer_mm_addr(), th::pubkey_a());
    // Writer MM funds its account with cash collateral.
    fund_account<USDC>(&mut scenario, th::writer_mm_addr(), 10_000_000);

    let write_amount: u64 = 80;
    let premium: u64 = 2_000_000;
    let collateral = 80 * (STRIKE as u64); // 4_000_000

    ts::next_tx(&mut scenario, th::trader_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);

    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::writer_mm_addr(), // signer (writer MM) recipient == position recipient
        object::id(&b),
        write_amount,
        premium,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);

    put_bucket::execute_write_for_testing<BTC, USDC, PUT>(
        &mut b,
        &config,
        &mut treasury,
        &mut mm_acc,
        coin::zero<USDC>(scenario.ctx()),
        coin::mint_for_testing<USDC>(premium, scenario.ctx()),
        bucket::trader_flow(),
        th::writer_mm_addr(), // position recipient = MM
        th::trader_addr(),    // put token recipient = retail trader
        sq,
        &clock,
        scenario.ctx(),
    );

    assert!(put_bucket::settlement_balance(&b) == collateral, 0);
    assert!(put_bucket::put_supply(&b) == write_amount, 0);
    // MM: collateral withdrawn, net premium deposited.
    assert!(account::balance_of<USDC>(&mm_acc) == 10_000_000 - collateral + premium, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // Trader gets the put coins.
    ts::next_tx(&mut scenario, th::trader_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    assert!(put.value() == write_amount, 0);
    ts::return_to_sender(&scenario, put);

    // Writer MM gets the Position.
    ts::next_tx(&mut scenario, th::writer_mm_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    assert!(position::range_end(&pos) == (write_amount as u128), 0);
    ts::return_to_sender(&scenario, pos);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- exercise ---

#[test]
fun test_exercise_happy_path() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 100, 1_000_000, 1);

    // Buyer (trader MM) holds the put; exercise 40 of it by delivering 40 BTC.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let exercise_chunk = coin::split(&mut put, 40, scenario.ctx());
    ts::return_to_sender(&scenario, put);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(40, scenario.ctx());
    let settlement = put_bucket::exercise<BTC, USDC, PUT>(
        &mut b,
        exercise_chunk,
        delivery,
        &clock,
        scenario.ctx(),
    );
    let expected_cash = (((40 as u128) * STRIKE) as u64);
    assert!(settlement.value() == expected_cash, 0);
    assert!(put_bucket::exercise_cursor(&b) == 40, 0);
    assert!(put_bucket::underlying_balance(&b) == 40, 0);
    assert!(put_bucket::settlement_balance(&b) == 100 * (STRIKE as u64) - expected_cash, 0);
    assert!(put_bucket::put_supply(&b) == 60, 0); // 100 − 40 burned

    coin::burn_for_testing(settlement);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 12, location = options_core::put_bucket)] // amount_mismatch
fun test_exercise_wrong_delivery_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(49, scenario.ctx()); // one short
    let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, put, delivery, &clock, scenario.ctx());
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 8, location = options_core::put_bucket)] // bucket_expired
fun test_exercise_after_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(50, scenario.ctx());
    let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, put, delivery, &clock, scenario.ctx());
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- redeem ---

#[test]
#[expected_failure(abort_code = 9, location = options_core::put_bucket)] // bucket_not_expired
fun test_redeem_before_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (u, s) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, pos, &clock, scenario.ctx());
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_fully_unexercised_returns_all_cash() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 80, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (u, s) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 0, 0);
    assert!(s.value() == 80 * (STRIKE as u64), 0);
    assert!(put_bucket::total_redeemed(&b) == 80, 0);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_redeem_fully_exercised_returns_all_underlying() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 60, 1_000, 1);

    // Holder exercises the whole lot.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(60, scenario.ctx());
    let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, put, delivery, &clock, scenario.ctx());
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (u, s) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 60, 0);
    assert!(s.value() == 0, 0);
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_fifo_two_writers_partial_exercise() {
    // Writes [0,100) then [100,150). Exercise 120 → first writer fully
    // exercised (gets 100 underlying), second 20/50 exercised.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 100_000_000);

    write_put_writer(&mut scenario, &clock, 100, 1_000, 1);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 2);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut put_a = ts::take_from_sender<Coin<PUT>>(&scenario);
    let put_b = ts::take_from_sender<Coin<PUT>>(&scenario);
    coin::join(&mut put_a, put_b);
    assert!(put_a.value() == 150, 0);
    let exercise_piece = coin::split(&mut put_a, 120, scenario.ctx());
    ts::return_to_sender(&scenario, put_a);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(120, scenario.ctx());
    let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, exercise_piece, delivery, &clock, scenario.ctx());
    assert!(put_bucket::exercise_cursor(&b) == 120, 0);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let pos_a = ts::take_from_sender<Position>(&scenario);
    let pos_b = ts::take_from_sender<Position>(&scenario);
    let (early, late) = if (position::range_end(&pos_a) == 100) { (pos_a, pos_b) } else { (pos_b, pos_a) };

    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);

    // Early writer [0,100): fully exercised → 100 underlying, 0 cash.
    let (u_early, s_early) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, early, &clock, scenario.ctx());
    assert!(u_early.value() == 100, 0);
    assert!(s_early.value() == 0, 0);
    coin::burn_for_testing(u_early);
    coin::burn_for_testing(s_early);

    // Late writer [100,150): 20 exercised → 20 underlying, 30 unexercised → 30×strike cash.
    let (u_late, s_late) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, late, &clock, scenario.ctx());
    assert!(u_late.value() == 20, 0);
    assert!(s_late.value() == 30 * (STRIKE as u64), 0);
    coin::burn_for_testing(u_late);
    coin::burn_for_testing(s_late);

    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- solvency under fractional strike ---

#[test]
fun test_solvency_fractional_strike_with_dust_sweep() {
    // strike 15_000 @ scale 5 ⇒ 0.15 cash per underlying-unit.
    //   write 21 ⇒ collateral ceil(3.15) = 4
    //   exercise 7 ⇒ floor(1.05) = 1 cash out (×2 = 14 exercised, 2 cash out)
    //   redeem [0,21): 14 underlying + floor(7×0.15)=1 cash
    //   total cash out = 1 + 1 + 1 = 3  <  collateral 4  ⇒ dust 1 (swept)
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_put_bucket<BTC, USDC, PUT>(&mut scenario, EXPIRY_MS, 15_000, 5);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    // Writer-flow write of 21 (collateral computed on-chain = 4).
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut treasury = th::take_treasury(&scenario);
    let mut mm_acc = th::take_account(&scenario);
    let collateral_amount = put_bucket::required_collateral(&b, 21);
    assert!(collateral_amount == 4, 0);
    let q = quote::new_quote(
        *admin::protocol_id(&config),
        object::id(&mm_acc),
        th::trader_mm_addr(),
        object::id(&b),
        21,
        1_000,
        EXPIRY_MS,
        1,
    );
    let sq = quote::new_signed_quote(q, vector[]);
    put_bucket::execute_write_for_testing<BTC, USDC, PUT>(
        &mut b, &config, &mut treasury, &mut mm_acc,
        coin::mint_for_testing<USDC>(collateral_amount, scenario.ctx()),
        coin::zero<USDC>(scenario.ctx()),
        bucket::writer_flow(),
        th::writer_addr(),
        th::trader_mm_addr(),
        sq, &clock, scenario.ctx(),
    );
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(treasury);
    ts::return_shared(mm_acc);

    // Two exercises of 7 each.
    let mut i = 0;
    while (i < 2) {
        ts::next_tx(&mut scenario, th::trader_mm_addr());
        let mut put = ts::take_from_sender<Coin<PUT>>(&scenario);
        let chunk = coin::split(&mut put, 7, scenario.ctx());
        ts::return_to_sender(&scenario, put);

        ts::next_tx(&mut scenario, th::trader_mm_addr());
        let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
        let delivery = coin::mint_for_testing<BTC>(7, scenario.ctx());
        let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, chunk, delivery, &clock, scenario.ctx());
        assert!(s.value() == 1, 0); // floor(1.05)
        coin::burn_for_testing(s);
        ts::return_shared(b);
        i = i + 1;
    };

    // Expire and redeem the single position.
    clock.set_for_testing(EXPIRY_MS + 1);
    ts::next_tx(&mut scenario, th::writer_addr());
    let pos = ts::take_from_sender<Position>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let (u, s) = put_bucket::redeem_position<BTC, USDC, PUT>(&mut b, pos, &clock, scenario.ctx());
    assert!(u.value() == 14, 0);  // exercised range underlying
    assert!(s.value() == 1, 0);   // floor(7 × 0.15)
    coin::burn_for_testing(u);
    coin::burn_for_testing(s);

    // Solvent: 1 cash unit of dust remains, underlying fully drained.
    assert!(put_bucket::underlying_balance(&b) == 0, 0);
    assert!(put_bucket::settlement_balance(&b) == 1, 0);
    assert!(put_bucket::total_redeemed(&b) == 21, 0);
    ts::return_shared(b);

    // Cleanup sweeps the 1 unit of dust to the admin.
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    put_bucket::cleanup_bucket<BTC, USDC, PUT>(&cap, b, &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);

    ts::next_tx(&mut scenario, th::admin_addr());
    let dust = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(dust.value() == 1, 0);
    coin::burn_for_testing(dust);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- cleanup guard ---

#[test]
#[expected_failure(abort_code = 10, location = options_core::put_bucket)] // bucket_not_drained
fun test_cleanup_before_all_redeemed_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    // Position never redeemed ⇒ total_redeemed (0) != total_written (50).
    put_bucket::cleanup_bucket<BTC, USDC, PUT>(&cap, b, &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- invalidation ---

#[test]
#[expected_failure(abort_code = 26, location = options_core::put_bucket)] // bucket_invalidated
fun test_invalidated_blocks_write() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    put_bucket::invalidate_bucket<BTC, USDC, PUT>(&cap, &mut b, b"halt", &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(b);

    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun test_exercise_works_when_invalidated() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    put_bucket::invalidate_bucket<BTC, USDC, PUT>(&cap, &mut b, b"halt", &clock, scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(b);

    // Exercise is unaffected by invalidation.
    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let delivery = coin::mint_for_testing<BTC>(50, scenario.ctx());
    let s = put_bucket::exercise<BTC, USDC, PUT>(&mut b, put, delivery, &clock, scenario.ctx());
    assert!(s.value() == 50 * (STRIKE as u64), 0);
    coin::burn_for_testing(s);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- burn expired ---

#[test]
fun test_burn_expired_option() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());
    fund_account<USDC>(&mut scenario, th::trader_mm_addr(), 10_000_000);
    write_put_writer(&mut scenario, &clock, 50, 1_000, 1);

    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut scenario, th::trader_mm_addr());
    let put = ts::take_from_sender<Coin<PUT>>(&scenario);
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    put_bucket::burn_expired_option<BTC, USDC, PUT>(&mut b, put, &clock, scenario.ctx());
    assert!(put_bucket::put_supply(&b) == 0, 0);
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}

// --- self-write primitive ---

#[test]
fun test_write_collateralized_self_write() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup_bucket(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let collateral = coin::mint_for_testing<USDC>(70 * (STRIKE as u64), scenario.ctx());
    let (pos, put) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut b, collateral, 70, &clock, scenario.ctx(),
    );
    assert!(position::range_end(&pos) == 70, 0);
    assert!(put.value() == 70, 0);
    assert!(put_bucket::settlement_balance(&b) == 70 * (STRIKE as u64), 0);
    coin::burn_for_testing(put);
    transfer::public_transfer(pos, th::writer_addr());
    ts::return_shared(b);

    clock.destroy_for_testing();
    ts::end(scenario);
}
