#[test_only]
module trading_vault::vault_mm_tests;

use std::string;
use sui::balance;
use sui::coin;
use sui::test_scenario as ts;

use options_core::collateral::{Self, CollateralRequest};
use options_core::quote;

use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};
use trading_vault::vault_mm;

fun request_for(
    vault: &TradingVault,
    source: ID,
    recipient: address,
    amount: u64,
): CollateralRequest<h::USDC> {
    let q = quote::new_quote(
        b"proto",
        object::id(vault), // signer id (unused here)
        source,
        @0x0,
        string::utf8(b"vault_mm"),
        recipient,
        object::id(vault), // bucket id placeholder
        amount,
        0,
        0,
        1,
    );
    collateral::new_request_for_testing<h::USDC>(q, amount, false)
}

#[test]
fun release_pulls_collateral_when_enabled_and_bound_to_vault() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    // Curator opts in.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    // A core-minted request naming this vault as source AND recipient
    // releases collateral.
    ts::next_tx(&mut sc, h::bob_addr());
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, vault_id.to_address(), 250_000);
    let funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    assert!(funds.value() == 250_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 750_000);
    balance::destroy_for_testing(funds);
    collateral::destroy_for_testing(req);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 3, location = trading_vault::vault_mm)]
fun release_rejected_when_disabled() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, vault_id.to_address(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 2, location = trading_vault::vault_mm)]
fun release_rejected_when_outputs_routed_elsewhere() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    // The curator's bot signs a quote routing outputs to the CURATOR:
    // theft attempt, refused.
    ts::next_tx(&mut sc, h::curator_addr());
    let vault_id = object::id(&v);
    let req = request_for(&v, vault_id, h::curator_addr(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}

#[test]
#[expected_failure(abort_code = 1, location = trading_vault::vault_mm)]
fun release_rejected_for_wrong_source() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_mm_release_enabled(&mut v, &cap, true);
    ts::return_to_sender(&sc, cap);

    ts::next_tx(&mut sc, h::bob_addr());
    let vault_id = object::id(&v);
    let other = object::id_from_address(@0xBEEF);
    let req = request_for(&v, other, vault_id.to_address(), 250_000);
    let _funds = vault_mm::release<h::USDC>(&mut v, &req, sc.ctx());
    abort 0
}
