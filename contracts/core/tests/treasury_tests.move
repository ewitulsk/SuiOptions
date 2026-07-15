#[test_only]
module options_core::treasury_tests;

use sui::coin;
use sui::test_scenario::{Self as ts};

use options_core::treasury;
use options_core::test_helpers::{Self as th, USDC};

#[test]
fun test_treasury_deposit_and_withdraw() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let mut treasury = th::take_treasury(&scenario);
    assert!(treasury::balance_of<USDC>(&treasury) == 0, 0);

    // Seed the treasury via the package-internal deposit (mirroring the fee skim path).
    let coin_in = coin::mint_for_testing<USDC>(500_000, scenario.ctx());
    treasury::deposit_balance(&mut treasury, coin_in.into_balance());
    assert!(treasury::balance_of<USDC>(&treasury) == 500_000, 0);

    let cap = th::take_admin_cap(&scenario);
    treasury::withdraw<USDC>(&cap, &mut treasury, 200_000, th::stranger_addr(), scenario.ctx());
    assert!(treasury::balance_of<USDC>(&treasury) == 300_000, 0);
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(treasury);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let received = ts::take_from_sender<coin::Coin<USDC>>(&scenario);
    assert!(received.value() == 200_000, 0);
    coin::burn_for_testing(received);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 20, location = options_core::treasury)] // insufficient_treasury_balance
fun test_treasury_withdraw_overdraw_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let mut treasury = th::take_treasury(&scenario);
    let coin_in = coin::mint_for_testing<USDC>(100, scenario.ctx());
    treasury::deposit_balance(&mut treasury, coin_in.into_balance());

    let cap = th::take_admin_cap(&scenario);
    treasury::withdraw<USDC>(&cap, &mut treasury, 101, th::stranger_addr(), scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(treasury);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 20, location = options_core::treasury)] // insufficient_treasury_balance — type with no deposits
fun test_treasury_withdraw_unknown_asset_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let mut treasury = th::take_treasury(&scenario);
    let cap = th::take_admin_cap(&scenario);
    treasury::withdraw<USDC>(&cap, &mut treasury, 1, th::stranger_addr(), scenario.ctx());
    th::return_admin_cap(&scenario, cap);
    ts::return_shared(treasury);

    clock.destroy_for_testing();
    ts::end(scenario);
}

