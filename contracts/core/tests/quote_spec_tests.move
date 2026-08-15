/// Spec-bound quoting (SO-408).
///
/// A quote names the bucket's ECONOMICS rather than its object id, so the
/// checks that used to be free — "is this the object I signed for?" — now have
/// to be earned field by field. These tests cover the ways a quote could be
/// redirected to a bucket its signer did not price:
///
///   * the same strike and expiry on the OTHER option kind (the expensive one:
///     a deep-ITM call quote filled with a near-worthless OTM put),
///   * the same strike and expiry on a different asset pair,
///   * and the queue bound, which is the assignment risk an object id only
///     ever implied.
///
/// The happy-path counterpart — two raw encodings of one strike resolving to
/// the same quote — is `normalized_strike_forms_are_interchangeable`.
#[test_only]
module options_core::quote_spec_tests;

use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin;
use options_core::bucket::{Self, Bucket};
use options_core::put_bucket::{Self, PutBucket};
use options_core::quote;
use options_core::test_helpers::{Self as th, BTC, USDC, CALL, PUT};

const EXPIRY_MS: u64 = 1_700_000_000_000;
const STRIKE: u128 = 50_000;
const STRIKE_SCALE: u8 = 0;

fun setup(scenario: &mut Scenario) {
    th::new_bucket<BTC, USDC, CALL>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::new_put_bucket<BTC, USDC, PUT>(scenario, EXPIRY_MS, STRIKE, STRIKE_SCALE);
    th::create_signer(scenario, th::trader_mm_addr(), th::pubkey_a());
}

/// Write `amount` into the call bucket through the writer flow, so
/// `total_written` moves and the queue bound has something to bind against.
fun write_call(scenario: &mut Scenario, clock: &sui::clock::Clock, amount: u64, nonce: u64) {
    ts::next_tx(scenario, th::writer_addr());
    let mut b = ts::take_shared<Bucket<BTC, USDC, CALL>>(scenario);
    let config = th::take_config(scenario);
    let wl = th::take_whitelist(scenario);
    let mut treasury = th::take_treasury(scenario);
    let mut signer = th::take_signer(scenario);

    let premium: u64 = 1_000;
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
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), clock,
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

// ── cross-kind: the attack that only exists under spec binding ──────────

#[test]
#[expected_failure(abort_code = 5, location = options_core::bucket)] // quote_spec_mismatch
fun put_quote_cannot_be_spent_on_a_call_bucket() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Identical pair, expiry and strike — only `is_put` differs. Under id
    // binding the two buckets had different addresses and this was impossible.
    let q = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        STRIKE_SCALE,
        /* is_put */ true,
        std::u128::max_value!(),
        100,
        1_000,
        EXPIRY_MS,
        1,
    );
    let _req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    abort 42
}

#[test]
#[expected_failure(abort_code = 5, location = options_core::put_bucket)] // quote_spec_mismatch
fun call_quote_cannot_be_spent_on_a_put_bucket() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    let q = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        STRIKE_SCALE,
        /* is_put */ false,
        std::u128::max_value!(),
        100,
        1_000,
        EXPIRY_MS,
        1,
    );
    let _req = put_bucket::request_writer_flow_for_testing<BTC, USDC, PUT>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    abort 42
}

// ── cross-pair ─────────────────────────────────────────────────────────

#[test]
#[expected_failure(abort_code = 5, location = options_core::bucket)] // quote_spec_mismatch
fun quote_for_another_pair_cannot_be_spent() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Settlement type differs; everything else matches the bucket exactly.
    let q = th::new_test_quote_spec<BTC, BTC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        STRIKE_SCALE,
        /* is_put */ false,
        std::u128::max_value!(),
        100,
        1_000,
        EXPIRY_MS,
        1,
    );
    let _req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    abort 42
}

// ── strike normalization ───────────────────────────────────────────────

#[test]
fun normalized_strike_forms_are_interchangeable() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    // 500_000 / 10^1 is the same economic strike as 50_000 / 10^0.
    th::new_bucket<BTC, USDC, CALL>(&mut scenario, EXPIRY_MS, STRIKE * 10, 1);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Signed in the NORMALIZED form; the bucket stores the raw one.
    let q = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        0,
        /* is_put */ false,
        std::u128::max_value!(),
        100,
        1_000,
        EXPIRY_MS,
        1,
    );
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    // Reaching here at all is the assertion: the spec matched.
    let (_q, amount, is_writer) = options_core::collateral::destroy_for_testing(req);
    assert!(amount == 1_000, 0);
    assert!(is_writer, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

// ── max_total_written queue bound ──────────────────────────────────────

#[test]
#[expected_failure(abort_code = 74, location = options_core::bucket)] // quote_queue_exceeded
fun queue_bound_refuses_a_deeper_queue_than_priced() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);
    write_call(&mut scenario, &clock, 100, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);
    assert!(bucket::total_written(&b) == 100, 0);

    // The signer priced assignment risk for at most 50 units ahead of it;
    // 100 are already written.
    let q = th::new_test_quote_bounded<BTC, USDC, CALL>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        /* max_total_written */ 50,
        10,
        1_000,
        EXPIRY_MS,
        2,
    );
    let _req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    abort 42
}

#[test]
fun queue_bound_admits_an_exactly_equal_queue() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);
    write_call(&mut scenario, &clock, 100, 1);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<Bucket<BTC, USDC, CALL>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Boundary: the bound is inclusive, so exactly 100 written is admissible.
    let q = th::new_test_quote_bounded<BTC, USDC, CALL>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        &b,
        /* max_total_written */ 100,
        10,
        1_000,
        EXPIRY_MS,
        2,
    );
    let req = bucket::request_writer_flow_for_testing<BTC, USDC, CALL>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    let (_q, amount, _is_writer) = options_core::collateral::destroy_for_testing(req);
    assert!(amount == 1_000, 0);

    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 74, location = options_core::put_bucket)] // quote_queue_exceeded
fun queue_bound_applies_to_puts_too() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, th::writer_addr());
    let b = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let config = th::take_config(&scenario);
    let mut signer = th::take_signer(&scenario);

    // Nothing written yet, so any bound below zero is impossible — instead
    // pin the bound at zero and confirm it is the WRITTEN total that binds,
    // not the write amount, by writing first.
    let q = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config),
        object::id(&signer),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        STRIKE_SCALE,
        /* is_put */ true,
        /* max_total_written */ 0,
        10,
        1_000,
        EXPIRY_MS,
        1,
    );
    let req = put_bucket::request_writer_flow_for_testing<BTC, USDC, PUT>(
        &b, &mut signer, &config, quote::new_signed_quote(q, vector[]), &clock,
    );
    // An empty bucket passes a zero bound: 0 <= 0.
    let (_q, _amount, _w) = options_core::collateral::destroy_for_testing(req);

    // Now write, and a second zero-bound quote must be refused.
    ts::return_shared(b);
    ts::return_shared(config);
    ts::return_shared(signer);
    let write_amount: u64 = 25;
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut b2 = ts::take_shared<PutBucket<BTC, USDC, PUT>>(&scenario);
    let wl = th::take_whitelist(&scenario);
    let collateral_in = coin::mint_for_testing<USDC>(
        put_bucket::required_collateral(&b2, write_amount),
        scenario.ctx(),
    );
    let (pos, puts) = put_bucket::write_collateralized<BTC, USDC, PUT>(
        &mut b2, &wl, collateral_in, write_amount, &clock, scenario.ctx(),
    );
    transfer::public_transfer(pos, th::writer_addr());
    transfer::public_transfer(puts, th::writer_addr());
    ts::return_shared(wl);

    let config2 = th::take_config(&scenario);
    let mut signer2 = th::take_signer(&scenario);
    let q2 = th::new_test_quote_spec<BTC, USDC>(
        *admin::protocol_id(&config2),
        object::id(&signer2),
        th::trader_mm_addr(),
        EXPIRY_MS,
        (STRIKE as u64),
        STRIKE_SCALE,
        /* is_put */ true,
        /* max_total_written */ 0,
        10,
        1_000,
        EXPIRY_MS,
        2,
    );
    let _req2 = put_bucket::request_writer_flow_for_testing<BTC, USDC, PUT>(
        &b2, &mut signer2, &config2, quote::new_signed_quote(q2, vector[]), &clock,
    );
    abort 42
}
