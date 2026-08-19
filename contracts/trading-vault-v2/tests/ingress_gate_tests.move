/// Guarded-launch ingress gate on the vault's money-entry points:
/// `create_vault` and both deposit paths route through
/// `whitelist::assert_ingress_allowed`. Exits (request_withdraw,
/// fulfillment) must keep working for de-listed members and while paused.
#[test_only]
module vault_v2::ingress_gate_tests;

use sui::test_scenario::{Self as ts};

use options_core::treasury::Treasury;
use whitelist::whitelist as wl_mod;

use vault_v2::test_helpers as h;
use vault_v2::vault::{Self, CuratorCap, TradingVault};

/// Never whitelisted.
const STRANGER: address = @0xF6;

fun remove_member(sc: &mut ts::Scenario, who: address) {
    ts::next_tx(sc, h::admin_addr());
    let cap = h::take_wl_admin_cap(sc);
    let mut wl = h::take_whitelist(sc);
    wl_mod::remove_member(&cap, &mut wl, who);
    ts::return_shared(wl);
    h::return_wl_admin_cap(sc, cap);
}

fun set_ingress_paused(sc: &mut ts::Scenario, paused: bool) {
    ts::next_tx(sc, h::admin_addr());
    let cap = h::take_wl_admin_cap(sc);
    let mut wl = h::take_whitelist(sc);
    wl_mod::set_ingress_paused(&cap, &mut wl, paused);
    ts::return_shared(wl);
    h::return_wl_admin_cap(sc, cap);
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun non_member_deposit_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc, &clock);
    h::simple_deposit(&mut sc, STRANGER, 1_000_000, &clock);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun non_member_create_vault_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);

    ts::next_tx(&mut sc, STRANGER);
    let cfg = h::take_protocol_config(&sc);
    let wl = h::take_whitelist(&sc);
    let _id = vault::create_vault<h::USDC>(
        &cfg,
        &wl,
        3_600_000,
        1_000,
        3_600_000,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
        b"spec-hash-test",
        &clock,
        sc.ctx(),
    );
    ts::return_shared(wl);
    ts::return_shared(cfg);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 1, location = whitelist::whitelist)] // EIngressRestricted
fun delisted_curator_cap_deposit_aborts() {
    // The cap-keyed commitment deposit path is gated on the SENDER too: a
    // curator removed from the whitelist cannot keep depositing via their
    // cap.
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc, &clock);

    remove_member(&mut sc, h::curator_addr());

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let wl = h::take_whitelist(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit_into_commitment<h::USDC>(
        &mut v,
        &cfg,
        &wl,
        &cap,
        appraisal,
        sui::coin::from_balance(h::mint<h::USDC>(1_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_to_sender(&sc, cap);
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 2, location = whitelist::whitelist)] // EIngressPaused
fun ingress_pause_blocks_member_deposit() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc, &clock);

    set_ingress_paused(&mut sc, true);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun pause_and_delisting_never_block_withdrawals() {
    // Alice deposits, then is removed from the whitelist AND ingress is
    // paused; her withdrawal (and bob's permissionless fulfillment crank)
    // still complete in full.
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc, &clock);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    remove_member(&mut sc, h::alice_addr());
    set_ingress_paused(&mut sc, true);

    clock.set_for_testing(4_000_000); // past lockup
    h::request_withdraw_all(&mut sc, h::alice_addr(), &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    assert!(vault::pending_withdrawals(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Alice received her full stake back (no profit → no fees).
    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<sui::coin::Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_000_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}
