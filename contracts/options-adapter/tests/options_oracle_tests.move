#[test_only]
module options_adapter::options_oracle_tests;

use std::type_name;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap};
use options_core::bucket;
use options_core::put_bucket;

use trading_vault::price as tv_price;
use trading_vault::registry as tv_registry;
use trading_vault::registry::OracleRegistry;

use options_adapter::options_oracle::{Self as oracle, OptionsOracle};

/// Underlying (8 decimals in spirit; decimals live in the price legs).
public struct UND has drop {}
/// Settlement == the vault quote asset in these tests.
public struct QUOTE has drop {}
public struct CALL has drop {}
public struct PUT has drop {}

/// Input-leg oracle for the U→Q attestation.
public struct TestOracle has drop {}

const ADMIN: address = @0xA1;
const EXPIRY_MS: u64 = 10_000_000;
/// Strike 2.0 QUOTE per UND raw unit, scale 12.
const STRIKE: u128 = 2_000_000_000_000;
const SCALE_1E12: u128 = 1_000_000_000_000;

fun setup(sc: &mut Scenario): Clock {
    ts::next_tx(sc, ADMIN);
    admin::init_for_testing(sc.ctx());
    tv_registry::init_for_testing(sc.ctx());

    ts::next_tx(sc, ADMIN);
    let admin_cap = ts::take_from_sender<AdminCap>(sc);
    let mut oreg = ts::take_shared<OracleRegistry>(sc);
    tv_registry::allow_oracle(&admin_cap, &mut oreg, type_name::with_defining_ids<TestOracle>());
    tv_registry::allow_oracle(
        &admin_cap,
        &mut oreg,
        type_name::with_defining_ids<OptionsOracle>(),
    );
    ts::return_shared(oreg);

    let call_tcap = coin::create_treasury_cap_for_testing<CALL>(sc.ctx());
    bucket::create_bucket<UND, QUOTE, CALL>(&admin_cap, call_tcap, EXPIRY_MS, STRIKE, 12, sc.ctx());
    let put_tcap = coin::create_treasury_cap_for_testing<PUT>(sc.ctx());
    put_bucket::create_put_bucket<UND, QUOTE, PUT>(&admin_cap, put_tcap, EXPIRY_MS, STRIKE, 12, sc.ctx());
    ts::return_to_sender(sc, admin_cap);

    ts::next_tx(sc, ADMIN);
    clock::create_for_testing(sc.ctx())
}

fun und_att(oreg: &OracleRegistry, price: u128, ts_ms: u64): tv_price::PriceAttestation {
    tv_price::attest(
        TestOracle {},
        oreg,
        type_name::with_defining_ids<UND>(),
        type_name::with_defining_ids<QUOTE>(),
        price,
        ts_ms,
    )
}

#[test]
fun itm_call_prices_at_intrinsic() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    clock.set_for_testing(5_000);

    ts::next_tx(&mut sc, ADMIN);
    // Spot 3.5 Q/UND, strike 2.0 → intrinsic 1.5 at 1e12.
    let b = ts::take_shared<bucket::Bucket<UND, QUOTE, CALL>>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let att = und_att(&oreg, 3_500_000_000_000, 4_000);
    let out = oracle::attest_call<UND, QUOTE, CALL, QUOTE>(
        &oreg,
        &b,
        option::some(att),
        option::none(), // S == Q → 1:1 leg
        &clock,
    );
    assert!(tv_price::price(&out) == 1_500_000_000_000);
    assert!(tv_price::asset(&out) == type_name::with_defining_ids<CALL>());
    assert!(tv_price::quote_asset(&out) == type_name::with_defining_ids<QUOTE>());
    // Timestamp is the weakest (oldest) leg — the U attestation.
    assert!(tv_price::timestamp_ms(&out) == 4_000);
    ts::return_shared(oreg);
    ts::return_shared(b);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun otm_call_prices_at_dust() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    clock.set_for_testing(5_000);

    ts::next_tx(&mut sc, ADMIN);
    // Spot 1.0 < strike 2.0 → dust floor 1.
    let b = ts::take_shared<bucket::Bucket<UND, QUOTE, CALL>>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let att = und_att(&oreg, SCALE_1E12, 5_000);
    let out = oracle::attest_call<UND, QUOTE, CALL, QUOTE>(
        &oreg,
        &b,
        option::some(att),
        option::none(),
        &clock,
    );
    assert!(tv_price::price(&out) == 1);
    ts::return_shared(oreg);
    ts::return_shared(b);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun expired_call_prices_at_dust_without_attestations() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    clock.set_for_testing(EXPIRY_MS + 1);

    ts::next_tx(&mut sc, ADMIN);
    let b = ts::take_shared<bucket::Bucket<UND, QUOTE, CALL>>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let out = oracle::attest_call<UND, QUOTE, CALL, QUOTE>(
        &oreg,
        &b,
        option::none(),
        option::none(),
        &clock,
    );
    assert!(tv_price::price(&out) == 1);
    assert!(tv_price::timestamp_ms(&out) == EXPIRY_MS + 1);
    ts::return_shared(oreg);
    ts::return_shared(b);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun itm_put_prices_at_intrinsic() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    clock.set_for_testing(5_000);

    ts::next_tx(&mut sc, ADMIN);
    // Spot 0.5 < strike 2.0 → put intrinsic 1.5.
    let b = ts::take_shared<put_bucket::PutBucket<UND, QUOTE, PUT>>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let att = und_att(&oreg, 500_000_000_000, 5_000);
    let out = oracle::attest_put<UND, QUOTE, PUT, QUOTE>(
        &oreg,
        &b,
        option::some(att),
        option::none(),
        &clock,
    );
    assert!(tv_price::price(&out) == 1_500_000_000_000);
    assert!(tv_price::asset(&out) == type_name::with_defining_ids<PUT>());
    ts::return_shared(oreg);
    ts::return_shared(b);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = options_adapter::options_oracle)]
fun live_call_without_underlying_attestation_aborts() {
    let mut sc = ts::begin(ADMIN);
    let mut clock = setup(&mut sc);
    clock.set_for_testing(5_000);

    ts::next_tx(&mut sc, ADMIN);
    let b = ts::take_shared<bucket::Bucket<UND, QUOTE, CALL>>(&sc);
    let oreg = ts::take_shared<OracleRegistry>(&sc);
    let out = oracle::attest_call<UND, QUOTE, CALL, QUOTE>(
        &oreg,
        &b,
        option::none(),
        option::none(),
        &clock,
    );
    let _ = tv_price::price(&out);
    abort 99
}

#[test]
fun strike_leg_scales_with_settlement_price() {
    // strike 2.0 (scale 12) × settlement at 0.5 Q → 1.0 Q per unit.
    assert!(
        oracle::strike_in_quote_for_testing(STRIKE, 12, 500_000_000_000) == SCALE_1E12,
    );
    // Degenerate scale 0: strike is a plain raw ratio.
    assert!(oracle::strike_in_quote_for_testing(3, 0, SCALE_1E12) == 3 * SCALE_1E12);
}
