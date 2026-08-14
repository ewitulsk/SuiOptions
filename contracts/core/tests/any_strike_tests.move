// Any-strike creation: runtime currencies (option_coin + coin_registry),
// derived bucket IDs, canonical strike encoding, and the full write →
// exercise → redeem lifecycle on a registry-created bucket.
//
// Test economics (chosen so every marker byte is hand-checkable):
//   expiry_ms = 3_000_000_000_000  → minutes = 50_000_000 = 0x02FAF080
//   strike raw (2571, 2)           → normalized sig = 2571 = 0x0A0B, exp = 2
#[test_only]
module options_core::any_strike_tests;

use sui::clock;
use sui::coin;
use sui::coin_registry;
use sui::derived_object;
use sui::test_scenario as ts;

use options_core::bucket;
use options_core::bucket_registry;
use options_core::enc0::{B00, B02, B03, B0A, B0B};
use options_core::enc1::{B80, BF0, BFA};
use options_core::option_coin::{Self, OptionCall};
use options_core::put_bucket;
use options_core::test_helpers as th;
use options_core::test_helpers::{BTC, USDC};

const EXPIRY_MS: u64 = 3_000_000_000_000;
const EXPIRY_MINUTES: u32 = 50_000_000;

/// `create_coin_data_registry_for_testing` requires the system sender, so
/// registries are minted in a dedicated @0x0 transaction — which also keeps
/// the expected_failure tests honest (their aborts come from the gate under
/// test, never from registry setup).
fun setup_registries(scenario: &mut ts::Scenario) {
    ts::next_tx(scenario, @0x0);
    bucket_registry::share_for_testing(bucket_registry::new_for_testing(scenario.ctx()));
    coin_registry::share_for_testing(
        coin_registry::create_coin_data_registry_for_testing(scenario.ctx()),
    );
}

#[test]
fun encoding_matches_type_name() {
    // The load-bearing empirical fact: the on-chain expected-string builder
    // reproduces `type_name::with_defining_ids` byte-for-byte (bare 64-hex
    // addresses, comma separator, no spaces).
    let actual = std::type_name::with_defining_ids<
        OptionCall<BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02>,
    >().into_string().into_bytes();
    let expected = option_coin::expected_type_bytes_for_testing<BTC, USDC>(
        b"OptionCall", EXPIRY_MINUTES, 2571, 2,
    );
    assert!(actual == expected, 0);
}

#[test]
fun normalize_strike_canonicalizes() {
    let (sig, exp) = option_coin::normalize_strike_for_testing(2571, 2);
    assert!(sig == 2571 && exp == 2, 0);
    // Trailing zeros strip until the exponent floors at zero.
    let (sig, exp) = option_coin::normalize_strike_for_testing(257100, 4);
    assert!(sig == 2571 && exp == 2, 1);
    let (sig, exp) = option_coin::normalize_strike_for_testing(1500, 1);
    assert!(sig == 150 && exp == 0, 2);
    let (sig, exp) = option_coin::normalize_strike_for_testing(7, 0);
    assert!(sig == 7 && exp == 0, 3);
}

#[test]
fun any_strike_full_lifecycle() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    // Create at an arbitrary strike, atomically with a covered write —
    // exactly the shape the frontend PTB uses (create → write → share).
    let mut b = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());

    // Derived ID: computable from (registry, spec) alone.
    let expected_addr = derived_object::derive_address(
        object::id(&breg),
        bucket_registry::key(
            std::type_name::with_defining_ids<BTC>(),
            std::type_name::with_defining_ids<USDC>(),
            EXPIRY_MS, 2571, 2, false,
        ),
    );
    assert!(object::id(&b).to_address() == expected_addr, 0);

    // Normalized economics stored on the bucket.
    assert!(bucket::strike(&b) == 2571 && bucket::strike_scale(&b) == 2, 1);
    assert!(bucket::expiry_ms(&b) == EXPIRY_MS, 2);

    // Write against the not-yet-shared bucket (same-PTB composition).
    let underlying = coin::mint_for_testing<BTC>(100, scenario.ctx());
    let (position, call) = bucket::write_collateralized(
        &mut b, &wl, underlying, &clock, scenario.ctx(),
    );
    assert!(call.value() == 100, 3);
    assert!(bucket::call_supply(&b) == 100, 4);

    // Exercise 100 units: round_half_up(100 × 2571 / 10^2) = 2571.
    let pay = coin::mint_for_testing<USDC>(2571, scenario.ctx());
    let out = bucket::exercise(&mut b, call, pay, &clock, scenario.ctx());
    assert!(out.value() == 100, 5);
    coin::burn_for_testing(out);

    // Redeem post-expiry: fully exercised range → all settlement.
    clock.set_for_testing(EXPIRY_MS);
    let (u_out, s_out) = bucket::redeem_position(&mut b, position, &clock, scenario.ctx());
    assert!(u_out.value() == 0, 6);
    assert!(s_out.value() == 2571, 7);
    coin::burn_for_testing(u_out);
    coin::burn_for_testing(s_out);

    bucket::share_bucket(b);
    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure] // derived_object: key already claimed
fun duplicate_spec_aborts_even_across_raw_forms() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    let b1 = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());
    bucket::share_bucket(b1);

    // Same economics under a different raw form: (257100, 4) normalizes to
    // (2571, 2) — the derived key collides and the claim aborts.
    let b2 = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 257100, 4, 8, &clock, scenario.ctx());
    bucket::share_bucket(b2);

    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::option_coin)] // encoding_mismatch
fun lying_encoding_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    // Exponent marker claims 3; the value arguments normalize to exp = 2.
    let b = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B03,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());
    bucket::share_bucket(b);

    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 73, location = options_core::bucket)] // expiry_not_aligned
fun unaligned_expiry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    let b = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS + 1, 2571, 2, 8, &clock, scenario.ctx());
    bucket::share_bucket(b);

    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
#[expected_failure] // whitelist ingress gate
fun stranger_cannot_create() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    let b = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());
    bucket::share_bucket(b);

    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun put_twin_creates_with_distinct_currency() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    setup_registries(&mut scenario);
    ts::next_tx(&mut scenario, th::writer_addr());
    let mut breg = ts::take_shared<bucket_registry::BucketRegistry>(&scenario);
    let mut creg = ts::take_shared<coin_registry::CoinRegistry>(&scenario);
    let wl = th::take_whitelist(&scenario);

    // Same spec, both sides: the call and put roots are distinct types, so
    // both currencies register and both derived keys (is_put flag) claim.
    let cb = bucket::create_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());
    let pb = put_bucket::create_put_bucket_any_strike<
        BTC, USDC, B02, BFA, BF0, B80, B00, B00, B00, B0A, B0B, B02,
    >(&mut breg, &mut creg, &wl, EXPIRY_MS, 2571, 2, 8, &clock, scenario.ctx());

    assert!(put_bucket::strike(&pb) == 2571 && put_bucket::strike_scale(&pb) == 2, 0);
    assert!(object::id(&cb) != object::id(&pb), 1);

    bucket::share_bucket(cb);
    put_bucket::share_put_bucket(pb);
    ts::return_shared(breg);
    ts::return_shared(creg);
    ts::return_shared(wl);
    clock.destroy_for_testing();
    scenario.end();
}
