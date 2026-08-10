#[test_only]
/// Multi-asset deposits/withdrawals (SO-370): oracle-valued entry,
/// chosen-asset payout, allowlist governance, queue liveness, haircuts,
/// and the share-inflation property the virtual offset exists for.
module trading_vault::multi_asset_tests;

use std::type_name;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::treasury::Treasury;

use trading_vault::test_helpers as h;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

/// 1 BTC-raw = 2 USDC-raw.
const BTC_PRICE: u128 = 2_000_000_000_000;

// Filler coin types for the allowlist-cap test.
public struct A1 has drop {}
public struct A2 has drop {}
public struct A3 has drop {}
public struct A4 has drop {}
public struct A5 has drop {}
public struct A6 has drop {}
public struct A7 has drop {}

fun allow_btc(sc: &mut Scenario) {
    ts::next_tx(sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(sc);
    let cfg = h::take_protocol_config(sc);
    let cap = ts::take_from_sender<CuratorCap>(sc);
    vault::add_deposit_asset<h::BTC>(&mut v, &cap, &cfg);
    ts::return_to_sender(sc, cap);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Deposit `amount` BTC as `who`, valued via a fresh attestation at
/// BTC_PRICE. Assumes the vault otherwise holds only USDC (no extra
/// appraisal legs).
fun btc_deposit(sc: &mut Scenario, who: address, amount: u64, clock: &Clock) {
    ts::next_tx(sc, who);
    let mut v = ts::take_shared<TradingVault>(sc);
    let cfg = h::take_protocol_config(sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    let att = h::attest<h::BTC, h::USDC>(sc, BTC_PRICE, clock.timestamp_ms());
    vault::deposit<h::BTC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(h::mint<h::BTC>(amount), sc.ctx()),
        option::some(att),
        clock,
        sc.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
}

// ═══════════════════════ deposit-side (entry) ═══════════════════════

#[test]
#[expected_failure(abort_code = 110, location = trading_vault::vault)]
fun deposit_unlisted_asset_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    btc_deposit(&mut sc, h::alice_addr(), 500_000, &clock);
    abort 0
}

#[test]
#[expected_failure(abort_code = 111, location = trading_vault::vault)]
fun non_accounting_deposit_without_attestation_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    allow_btc(&mut sc);

    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::deposit<h::BTC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(h::mint<h::BTC>(500_000), sc.ctx()),
        option::none(),
        &clock,
        sc.ctx(),
    );
    abort 0
}

#[test]
#[expected_failure(abort_code = 84, location = trading_vault::vault)]
fun stale_attestation_aborts_deposit() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    allow_btc(&mut sc);
    clock.set_for_testing(100_000); // attestation at t=0 is > 60s old

    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, 0);
    vault::deposit<h::BTC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(h::mint<h::BTC>(500_000), sc.ctx()),
        option::some(att),
        &clock,
        sc.ctx(),
    );
    abort 0
}

#[test]
fun genesis_deposit_in_non_accounting_mints_at_value() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    allow_btc(&mut sc);
    btc_deposit(&mut sc, h::alice_addr(), 500_000, &clock);

    ts::next_tx(&mut sc, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (shares, basis, _) = vault::stake_of(&v, h::alice_addr());
    // value = 500_000 × 2 = 1_000_000 → genesis mints value × O.
    assert!(shares == 1_000_000_000_000);
    assert!(basis == 1_000_000);
    assert!(vault::free_balance_of<h::BTC>(&v) == 500_000);
    assert!(vault::free_balance_of<h::USDC>(&v) == 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun priced_deposit_is_pps_neutral_for_existing_holders() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);
    // Bob deposits 500k BTC-raw = 1M value at pps exactly 1 → the offset
    // cancels and he matches Alice's stake exactly.
    btc_deposit(&mut sc, h::bob_addr(), 500_000, &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (bob_shares, bob_basis, _) = vault::stake_of(&v, h::bob_addr());
    assert!(bob_shares == 1_000_000_000_000);
    assert!(bob_basis == 1_000_000);
    assert!(vault::total_shares(&v) == 2_000_000_000_000);
    // Alice's crystallizable value is unchanged by Bob's entry: NAV 2M
    // over 2e12 shares is the same pps as 1M over 1e12.
    let alice_value = h::expected_value(1_000_000_000_000, 2_000_000_000_000, 2_000_000);
    assert!(alice_value >= 999_999 && alice_value <= 1_000_000);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun entry_haircut_reduces_credited_value() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    allow_btc(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_haircuts(&mut v, &cap, 100, 0); // 1% entry
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    btc_deposit(&mut sc, h::alice_addr(), 500_000, &clock);

    ts::next_tx(&mut sc, h::alice_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (shares, basis, _) = vault::stake_of(&v, h::alice_addr());
    // value = 1_000_000 × 99% = 990_000.
    assert!(basis == 990_000);
    assert!(shares == 990_000_000_000);
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════════════ allowlist governance ═══════════════════

#[test]
#[expected_failure(abort_code = 70, location = trading_vault::vault)]
fun stale_cap_cannot_manage_allowlist() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::rotate_curator_by_curator(&mut v, &cap, h::bob_addr(), sc.ctx());
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let old_cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::add_deposit_asset<h::BTC>(&mut v, &old_cap, &cfg);
    abort 0
}

#[test]
#[expected_failure(abort_code = 90, location = trading_vault::vault)]
fun allowlist_cap_binds() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    // Accounting + 7 fillers = 8 = DEFAULT_MAX_DEPOSIT_ASSETS.
    vault::add_deposit_asset<A1>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A2>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A3>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A4>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A5>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A6>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<A7>(&mut v, &cap, &cfg);
    vault::add_deposit_asset<h::BTC>(&mut v, &cap, &cfg); // 9th
    abort 0
}

#[test]
#[expected_failure(abort_code = 90, location = trading_vault::vault)]
fun accounting_asset_cannot_be_removed() {
    let mut sc = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::remove_deposit_asset<h::USDC>(&mut v, &cap);
    abort 0
}

#[test]
#[expected_failure(abort_code = 110, location = trading_vault::vault)]
fun removed_asset_cannot_deposit() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    allow_btc(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::remove_deposit_asset<h::BTC>(&mut v, &cap);
    assert!(!vault::is_deposit_asset(&v, &type_name::with_defining_ids<h::BTC>()));
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    btc_deposit(&mut sc, h::alice_addr(), 500_000, &clock);
    abort 0
}

// ═══════════════════ payout-side (exit) ═══════════════════

#[test]
fun payout_in_second_asset_pays_units_at_price() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);
    btc_deposit(&mut sc, h::bob_addr(), 500_000, &clock);

    // Alice exits her full stake, paid in BTC.
    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // Permissionless crank: appraise (BTC leg) + fulfillment potato.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, clock.timestamp_ms());
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, att, &clock);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[att], &clock);
    assert!(vault::fulfill_next<h::BTC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    vault::end_fulfillment(&v, f);
    // value 1M (no profit, no fee) → 1M / 2 = 500_000 BTC-raw.
    assert!(vault::pending_withdrawals(&v) == 0);
    assert!(vault::free_balance_of<h::BTC>(&v) == 0);
    assert!(vault::free_balance_of<h::USDC>(&v) == 1_000_000);
    assert!(vault::total_shares(&v) == 1_000_000_000_000);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::BTC>>(&sc, h::alice_addr());
    assert!(paid.value() == 500_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun mixed_asset_queue_pays_in_one_batch() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);
    btc_deposit(&mut sc, h::bob_addr(), 500_000, &clock);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // One potato, two typed fulfills, FIFO across asset types.
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, clock.timestamp_ms());
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, att, &clock);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[att], &clock);
    // Head (Alice, BTC): a USDC call is a no-op first — order enforced.
    assert!(!vault::fulfill_next<h::USDC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    assert!(vault::fulfill_next<h::BTC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    assert!(vault::fulfill_next<h::USDC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    vault::end_fulfillment(&v, f);
    assert!(vault::pending_withdrawals(&v) == 0);
    assert!(vault::total_shares(&v) == 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid_a = ts::take_from_address<Coin<h::BTC>>(&sc, h::alice_addr());
    assert!(paid_a.value() == 500_000);
    ts::return_to_address(h::alice_addr(), paid_a);
    let paid_b = ts::take_from_address<Coin<h::USDC>>(&sc, h::bob_addr());
    // Bob's value carries one unit of offset dust at this ratio.
    assert!(paid_b.value() >= 999_999 && paid_b.value() <= 1_000_000);
    ts::return_to_address(h::bob_addr(), paid_b);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun wedged_head_amend_unwedges() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);

    // Alice asks for BTC the vault does not hold.
    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // Crank can't fund BTC: no-op, request stays queued.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, clock.timestamp_ms());
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[att], &clock);
    assert!(!vault::fulfill_next<h::BTC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    vault::end_fulfillment(&v, f);
    assert!(vault::pending_withdrawals(&v) == 1);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Alice amends to the accounting asset; the crank now pays her.
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::amend_payout_asset<h::USDC>(&mut v, 0, sc.ctx());
    let (_, _, _, payout_asset, _) = vault::queue_request(&v, 0);
    assert!(payout_asset == type_name::with_defining_ids<h::USDC>());
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

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_000_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 89, location = trading_vault::vault)]
fun amend_by_non_recipient_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::amend_payout_asset<h::USDC>(&mut v, 0, sc.ctx());
    abort 0
}

#[test]
fun grace_fallback_pays_accounting_after_unwind_grace() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // Before the grace threshold the accounting fallback is refused.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[], &clock);
    assert!(!vault::fulfill_next<h::USDC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    vault::end_fulfillment(&v, f);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Head aged past unwind_grace_ms (1h): anyone pays it in USDC.
    clock.set_for_testing(4_000_000 + 3_600_000 + 1);
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

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(paid.value() == 1_000_000);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
#[expected_failure(abort_code = 111, location = trading_vault::vault)]
fun fulfill_without_batch_price_aborts() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);
    btc_deposit(&mut sc, h::bob_addr(), 500_000, &clock);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    // Potato without a BTC price: paying the BTC head is a composer bug.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, clock.timestamp_ms());
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, att, &clock);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[], &clock);
    let _ = vault::fulfill_next<h::BTC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx());
    abort 0
}

#[test]
fun exit_haircut_pays_fewer_units() {
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);
    btc_deposit(&mut sc, h::bob_addr(), 500_000, &clock);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::set_haircuts(&mut v, &cap, 0, 100); // 1% exit
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::BTC>(&mut v, 1_000_000_000_000, &clock, sc.ctx());
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let att = h::attest<h::BTC, h::USDC>(&sc, BTC_PRICE, clock.timestamp_ms());
    let mut appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::appraise_balance<h::BTC>(&v, &cfg, &mut appraisal, att, &clock);
    let mut f = vault::begin_fulfillment(&v, &cfg, appraisal, vector[att], &clock);
    assert!(vault::fulfill_next<h::BTC>(&mut v, &cfg, &mut treasury, &mut f, &clock, sc.ctx()));
    vault::end_fulfillment(&v, f);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::alice_addr());
    let paid = ts::take_from_address<Coin<h::BTC>>(&sc, h::alice_addr());
    // 1_000_000 value / (2 × 1.01) = 495_049 (floor).
    assert!(paid.value() == 495_049);
    ts::return_to_address(h::alice_addr(), paid);

    clock.destroy_for_testing();
    sc.end();
}

#[test]
fun closed_stake_enqueues_accounting_payout() {
    let mut sc = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);
    h::simple_deposit(&mut sc, h::alice_addr(), 1_000_000, &clock);
    allow_btc(&mut sc);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cap = ts::take_from_sender<CuratorCap>(&sc);
    vault::initiate_close(&mut v, &cap);
    vault::finalize_close(&mut v);
    ts::return_to_sender(&sc, cap);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::enqueue_closed_stake(&mut v, h::alice_addr(), &clock);
    let (_, _, _, payout_asset, _) = vault::queue_request(&v, 0);
    assert!(payout_asset == type_name::with_defining_ids<h::USDC>());
    ts::return_shared(v);

    clock.destroy_for_testing();
    sc.end();
}

// ═══════════════ share-inflation property (the offset's job) ═══════════════

#[test]
fun donation_inflation_attack_is_unprofitable() {
    // The classic first-depositor attack: tiny genesis stake, big
    // donation into NAV, victim's mint truncated. With the virtual
    // offset the donation accrues overwhelmingly to virtual shares —
    // the attacker must lose money, and the victim's loss is dust.
    let mut sc = ts::begin(h::admin_addr());
    let mut clock = h::init_protocol(&mut sc);
    h::new_default_vault(&mut sc);

    // Attacker's 1-unit genesis deposit.
    h::simple_deposit(&mut sc, h::alice_addr(), 1, &clock);
    // Donation lever (stand-in for any value inflow that mints no
    // shares — e.g. a fill overpaying the vault's own quoted price).
    h::session_gain<h::USDC>(&mut sc, 10_000_000);
    // Victim deposits.
    h::simple_deposit(&mut sc, h::bob_addr(), 2_000_000, &clock);

    ts::next_tx(&mut sc, h::bob_addr());
    let v = ts::take_shared<TradingVault>(&sc);
    let (victim_shares, _, _) = vault::stake_of(&v, h::bob_addr());
    assert!(victim_shares > 0);
    ts::return_shared(v);

    // Attacker exits everything.
    clock.set_for_testing(4_000_000);
    ts::next_tx(&mut sc, h::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let (attacker_shares, _, _) = vault::stake_of(&v, h::alice_addr());
    vault::request_withdraw<h::USDC>(&mut v, attacker_shares, &clock, sc.ctx());
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    // Attacker spent 10_000_001 (deposit + donation) — the exit must
    // come back far below that for the attack to be dead. (Fees eat
    // into it further; the bound below is generous.)
    ts::next_tx(&mut sc, h::alice_addr());
    let attacker_paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::alice_addr());
    assert!(attacker_paid.value() < 6_000_000);
    ts::return_to_address(h::alice_addr(), attacker_paid);

    // Victim exits nearly whole — loss bounded by dust, not by the
    // attacker's donation.
    ts::next_tx(&mut sc, h::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    vault::request_withdraw<h::USDC>(&mut v, victim_shares, &clock, sc.ctx());
    ts::return_shared(v);
    ts::next_tx(&mut sc, h::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&sc);
    let cfg = h::take_protocol_config(&sc);
    let mut treasury = ts::take_shared<Treasury>(&sc);
    let appraisal = vault::begin_appraisal<h::USDC>(&v);
    vault::fulfill_withdrawals<h::USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, sc.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut sc, h::bob_addr());
    let victim_paid = ts::take_from_address<Coin<h::USDC>>(&sc, h::bob_addr());
    assert!(victim_paid.value() >= 1_999_000);
    ts::return_to_address(h::bob_addr(), victim_paid);

    clock.destroy_for_testing();
    sc.end();
}
