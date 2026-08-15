#[test_only]
module mm_collateral::mm_collateral_tests;

use std::string;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::collateral;
use options_core::quote;

use mm_collateral::mm_collateral::{Self as mmc, CollateralAccount};

public struct USDC has drop {}

const MM: address = @0xA11CE;
const STRANGER: address = @0xB0B;

fun test_quote(source: ID, premium: u64): quote::Quote {
    // The bucket the quote names is irrelevant to `release`, which only
    // checks `source`; any well-formed spec does.
    quote::new_quote<USDC, USDC>(
        b"test-protocol",
        object::id_from_address(@0x51),
        source,
        @0xFACADE,
        string::utf8(b"mm_collateral"),
        MM,
        1_700_000_000_000, // expiry_ms
        50_000,            // strike significand
        0,                 // strike exponent
        false,             // is_put
        std::u128::max_value!(), // max_total_written
        10,
        premium,
        1_000_000,
        7,
    )
}

fun setup(scenario: &mut Scenario) {
    ts::next_tx(scenario, MM);
    mmc::init_for_testing(scenario.ctx());
    ts::next_tx(scenario, MM);
    let mut acct = ts::take_shared<CollateralAccount>(scenario);
    mmc::deposit(&mut acct, coin::mint_for_testing<USDC>(1_000_000, scenario.ctx()));
    ts::return_shared(acct);
}

#[test]
fun test_release_exact_amount_against_own_request() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, STRANGER); // release is executor-callable
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    let request = collateral::new_request_for_testing<USDC>(
        test_quote(object::id(&acct), 250_000),
        250_000,
        true,
        object::id_from_address(@0xB0C4),
    );
    let funds = mmc::release(&mut acct, &request, scenario.ctx());
    assert!(funds.value() == 250_000);
    assert!(mmc::balance_of<USDC>(&acct) == 750_000);
    ts::return_shared(acct);

    // Dispose of the potato + funds (a real caller passes them to core).
    let (_q, _amt, _w) = collateral::destroy_for_testing(request);
    transfer::public_transfer(
        coin::from_balance(funds, scenario.ctx()), MM);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 2, location = mm_collateral::mm_collateral)] // E_WRONG_ACCOUNT
fun test_release_foreign_request_aborts() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, STRANGER);
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    // Request naming a DIFFERENT collateral source.
    let request = collateral::new_request_for_testing<USDC>(
        test_quote(object::id_from_address(@0xDEAD), 250_000),
        250_000,
        true,
        object::id_from_address(@0xB0C4),
    );
    let _funds = mmc::release(&mut acct, &request, scenario.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 3, location = mm_collateral::mm_collateral)] // E_INSUFFICIENT_BALANCE
fun test_release_beyond_balance_aborts() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, STRANGER);
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    let request = collateral::new_request_for_testing<USDC>(
        test_quote(object::id(&acct), 2_000_000),
        2_000_000,
        true,
        object::id_from_address(@0xB0C4),
    );
    let _funds = mmc::release(&mut acct, &request, scenario.ctx());
    abort 0
}

#[test]
fun test_owner_withdraw() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, MM);
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    let c = mmc::withdraw<USDC>(&mut acct, 400_000, scenario.ctx());
    assert!(c.value() == 400_000);
    assert!(mmc::balance_of<USDC>(&acct) == 600_000);
    transfer::public_transfer(c, MM);
    ts::return_shared(acct);
    scenario.end();
}

#[test]
#[expected_failure(abort_code = 1, location = mm_collateral::mm_collateral)] // E_NOT_OWNER
fun test_stranger_withdraw_aborts() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, STRANGER);
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    let _c = mmc::withdraw<USDC>(&mut acct, 1, scenario.ctx());
    abort 0
}

#[test]
fun test_permissionless_deposit() {
    let mut scenario = ts::begin(MM);
    setup(&mut scenario);

    ts::next_tx(&mut scenario, STRANGER);
    let mut acct = ts::take_shared<CollateralAccount>(&scenario);
    mmc::deposit(&mut acct, coin::mint_for_testing<USDC>(5, scenario.ctx()));
    assert!(mmc::balance_of<USDC>(&acct) == 1_000_005);
    ts::return_shared(acct);
    scenario.end();
}
