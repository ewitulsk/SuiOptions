/// Guarded-launch ingress gate on the vault's money-entry points:
/// `create_vault` and both deposit paths route through
/// `options_core::admin::assert_ingress_allowed`. Exits (request_withdraw,
/// fulfillment) must keep working for de-listed members and while paused.
#[test_only]
module trading_vault::ingress_gate_tests;

use sui::test_scenario::{Self as ts};

use options_core::admin::{Self as core_admin, ProtocolConfig as CoreProtocolConfig};
use options_core::treasury::Treasury;

use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

/// Never whitelisted.
const STRANGER: address = @0xF6;

fun remove_member(sc: &mut ts::Scenario, who: address) {
    ts::next_tx(sc, h::admin_addr());
    let cap = h::take_admin_cap(sc);
    let mut core_cfg = ts::take_shared<CoreProtocolConfig>(sc);
    core_admin::remove_member(&cap, &mut core_cfg, who);
    ts::return_shared(core_cfg);
    h::return_admin_cap(sc, cap);
}

fun set_ingress_paused(sc: &mut ts::Scenario, paused: bool) {
    ts::next_tx(sc, h::admin_addr());
    let cap = h::take_admin_cap(sc);
    let mut core_cfg = ts::take_shared<CoreProtocolConfig>(sc);
    core_admin::set_ingress_paused(&cap, &mut core_cfg, paused);
    ts::return_shared(core_cfg);
    h::return_admin_cap(sc, cap);
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun non_member_deposit_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, STRANGER, 1_000_000, &clock);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun non_member_create_vault_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);

    ts::next_tx(&mut sc, STRANGER);
    let cfg = h::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    vault::create_vault<h::USDC>(&cfg, &core_cfg, 3_600_000, 1_000, 3_600_000, sc.ctx());
    ts::return_shared(core_cfg);
    ts::return_shared(cfg);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 71, location = options_core::admin)] // ingress_restricted
fun delisted_curator_cap_deposit_aborts() {
    // The cap-keyed deposit path is gated on the SENDER too: a curator
    // removed from the whitelist cannot keep depositing via their cap.
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    remove_member(&mut sc, h::curator_addr());

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let core_cfg = ts::take_shared<CoreProtocolConfig>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit_as_curator<h::USDC>(
        &mut v,
        &cfg,
        &core_cfg,
        &cap,
        appraisal,
        sui::coin::from_balance(h::mint<h::USDC>(1_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    ts::return_to_sender(&sc, cap);
    ts::return_shared(core_cfg);
    ts::return_shared(cfg);
    ts::return_shared(v);
    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 72, location = options_core::admin)] // ingress_paused
fun ingress_pause_blocks_member_deposit() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

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
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);

    remove_member(&mut sc, h::alice_addr());
    set_ingress_paused(&mut sc, true);

    clock.set_for_testing(4_000_000); // past lockup
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

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
