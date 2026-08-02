#[test_only]
/// Per-asset oracle pinning (SO-335).
///
/// The allowlist answers "may this adapter attest at all"; a pin narrows
/// that to "…for this asset". These tests fix the interaction between the
/// two, because getting it wrong in either direction is bad: too loose
/// and a second provider silently gains authority over the whole book,
/// too strict and an unpinned asset becomes unpriceable.
module trading_vault::oracle_pin_tests;

use std::type_name;
use sui::test_scenario::{Self as ts};

use options_core::admin::AdminCap;

use trading_vault::price;
use trading_vault::registry::{Self, OracleRegistry};
use trading_vault::test_helpers::{Self as h, BTC, USDC, TestOracle};

/// A second allowlisted adapter, standing in for "the other provider".
public struct OtherOracle has drop {}

const TS: u64 = 1_000_000;
const PRICE: u128 = 500_000_000_000_000;

fun allow_other(scenario: &mut ts::Scenario) {
    ts::next_tx(scenario, h::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(scenario);
    registry::allow_oracle(&cap, &mut oreg, type_name::with_defining_ids<OtherOracle>());
    ts::return_shared(oreg);
    ts::return_to_sender(scenario, cap);
}

fun pin<Asset, Oracle>(scenario: &mut ts::Scenario) {
    ts::next_tx(scenario, h::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(scenario);
    registry::pin_oracle(
        &cap,
        &mut oreg,
        type_name::with_defining_ids<Asset>(),
        type_name::with_defining_ids<Oracle>(),
    );
    ts::return_shared(oreg);
    ts::return_to_sender(scenario, cap);
}

/// Attest BTC→USDC with `OtherOracle` (the helper's `attest` uses
/// `TestOracle`).
fun attest_other(scenario: &ts::Scenario) {
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let _att = price::attest(
        OtherOracle {},
        &oreg,
        type_name::with_defining_ids<BTC>(),
        type_name::with_defining_ids<USDC>(),
        PRICE,
        TS,
    );
    ts::return_shared(oreg);
}

#[test]
fun unpinned_asset_accepts_any_allowlisted_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);

    // No pin ⇒ both adapters may price BTC. This is the default and the
    // pre-SO-335 behaviour; regressing it would break every deployment
    // that never configures pins.
    ts::next_tx(&mut scenario, h::admin_addr());
    let a = h::attest<BTC, USDC>(&scenario, PRICE, TS);
    assert!(price::price(&a) == PRICE);

    ts::next_tx(&mut scenario, h::admin_addr());
    attest_other(&scenario);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun pinned_asset_accepts_the_pinned_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, TestOracle>(&mut scenario);

    ts::next_tx(&mut scenario, h::admin_addr());
    let a = h::attest<BTC, USDC>(&scenario, PRICE, TS);
    assert!(price::price(&a) == PRICE);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 109)]
fun pinned_asset_rejects_a_different_allowlisted_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, TestOracle>(&mut scenario);

    // OtherOracle is allowlisted protocol-wide but not for BTC — this is
    // the whole point of pins, and it must abort distinctly from the
    // plain not-allowlisted case (76).
    ts::next_tx(&mut scenario, h::admin_addr());
    attest_other(&scenario);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun a_pin_is_per_asset_not_global() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, TestOracle>(&mut scenario);

    // USDC is unpinned, so OtherOracle may still price it even though
    // BTC is locked to TestOracle. This is what makes an incremental,
    // asset-by-asset migration possible.
    ts::next_tx(&mut scenario, h::admin_addr());
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let _att = price::attest(
        OtherOracle {},
        &oreg,
        type_name::with_defining_ids<USDC>(),
        type_name::with_defining_ids<BTC>(),
        PRICE,
        TS,
    );
    ts::return_shared(oreg);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun repinning_moves_the_asset_to_the_new_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, TestOracle>(&mut scenario);
    // The cutover itself: one call repoints the asset.
    pin<BTC, OtherOracle>(&mut scenario);

    ts::next_tx(&mut scenario, h::admin_addr());
    attest_other(&scenario);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun unpinning_restores_any_allowlisted_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, TestOracle>(&mut scenario);

    ts::next_tx(&mut scenario, h::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(&scenario);
    assert!(registry::has_oracle_pin(&oreg, &type_name::with_defining_ids<BTC>()));
    registry::unpin_oracle(&cap, &mut oreg, type_name::with_defining_ids<BTC>());
    assert!(!registry::has_oracle_pin(&oreg, &type_name::with_defining_ids<BTC>()));
    ts::return_shared(oreg);
    ts::return_to_sender(&scenario, cap);

    ts::next_tx(&mut scenario, h::admin_addr());
    attest_other(&scenario);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 76)]
fun cannot_pin_to_an_unallowlisted_oracle() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);

    // OtherOracle was never allowlisted. Pinning to it would make BTC
    // permanently unpriceable, so the registry refuses.
    pin<BTC, OtherOracle>(&mut scenario);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun disallowing_the_pinned_oracle_still_blocks_attestation() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);
    allow_other(&mut scenario);
    pin<BTC, OtherOracle>(&mut scenario);

    // Delisting is the kill switch and must win over a pin: the pin
    // survives, but `is_oracle_allowed_for` is false because the
    // allowlist check runs first.
    ts::next_tx(&mut scenario, h::admin_addr());
    let cap = ts::take_from_sender<AdminCap>(&scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(&scenario);
    registry::disallow_oracle(&cap, &mut oreg, type_name::with_defining_ids<OtherOracle>());
    let other = type_name::with_defining_ids<OtherOracle>();
    let btc = type_name::with_defining_ids<BTC>();
    assert!(registry::has_oracle_pin(&oreg, &btc));
    assert!(!registry::is_oracle_allowed_for(&oreg, &other, &btc));
    ts::return_shared(oreg);
    ts::return_to_sender(&scenario, cap);

    clock.destroy_for_testing();
    ts::end(scenario);
}
