#[test_only]
module options_protocol::account_tests;

use sui::clock;
use sui::coin;
use sui::test_scenario::{Self as ts};

use options_protocol::account::{Self, Account};
use options_protocol::test_helpers::{Self as th, USDC};

#[test]
fun test_create_and_share_account() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let acc = th::take_account(&scenario);
    assert!(account::owner(&acc) == th::writer_addr(), 0);
    assert!(*account::signing_pubkey(&acc) == th::pubkey_a(), 0);
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
fun test_deposit_and_withdraw_round_trip() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    let coin_in = coin::mint_for_testing<USDC>(1_000_000, scenario.ctx());
    account::deposit(&mut acc, coin_in);
    assert!(account::balance_of<USDC>(&acc) == 1_000_000, 0);
    ts::return_shared(acc);

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    let out = account::withdraw<USDC>(&mut acc, 400_000, scenario.ctx());
    assert!(out.value() == 400_000, 0);
    assert!(account::balance_of<USDC>(&acc) == 600_000, 0);
    coin::burn_for_testing(out);
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 15, location = options_protocol::account)] // not_owner
fun test_withdraw_not_owner_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    let coin_in = coin::mint_for_testing<USDC>(1_000_000, scenario.ctx());
    account::deposit(&mut acc, coin_in);
    ts::return_shared(acc);

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut acc = th::take_account(&scenario);
    let out = account::withdraw<USDC>(&mut acc, 1, scenario.ctx());
    coin::burn_for_testing(out);
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 11, location = options_protocol::account)] // insufficient_account_balance
fun test_withdraw_without_balance_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    let out = account::withdraw<USDC>(&mut acc, 1, scenario.ctx());
    coin::burn_for_testing(out);
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 15, location = options_protocol::account)] // not_owner
fun test_set_signing_key_not_owner_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut acc = th::take_account(&scenario);
    account::set_quote_signing_key(&mut acc, th::pubkey_b(), scenario.ctx());
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
fun test_set_signing_key_owner_succeeds() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    account::set_quote_signing_key(&mut acc, th::pubkey_b(), scenario.ctx());
    assert!(*account::signing_pubkey(&acc) == th::pubkey_b(), 0);
    ts::return_shared(acc);
    ts::end(scenario);
}

#[test]
fun test_prune_nonce_after_expiry() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());
    let mut clk = clock::create_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    account::consume_nonce(&mut acc, 7, 1000);
    assert!(account::has_nonce(&acc, 7), 0);

    clk.set_for_testing(1001);
    account::prune_nonce(&mut acc, 7, &clk);
    assert!(!account::has_nonce(&acc, 7), 0);

    ts::return_shared(acc);
    clk.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 19, location = options_protocol::account)] // nonce_still_valid
fun test_prune_nonce_before_expiry_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_account(&mut scenario, th::writer_addr(), th::pubkey_a());
    let mut clk = clock::create_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut acc = th::take_account(&scenario);
    account::consume_nonce(&mut acc, 9, 5000);

    clk.set_for_testing(1000);
    account::prune_nonce(&mut acc, 9, &clk);

    ts::return_shared(acc);
    clk.destroy_for_testing();
    ts::end(scenario);
}
