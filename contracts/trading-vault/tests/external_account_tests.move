/// External-account primitives: registration, budgeted + rate-limited
/// releases, sender-gated returns, and the mandatory equity leg in every
/// appraisal.
#[test_only]
module trading_vault::external_account_tests;

use std::type_name;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use trading_vault::test_helpers::{Self as th, USDC, TestOracle, RogueOracle};
use trading_vault::registry::OracleRegistry;
use trading_vault::vault::{Self, CuratorCap, TradingVault};

const DAY_MS: u64 = 86_400_000;

fun external_addr(): address { @0xF00D }

/// Register the external account on the default vault: TestOracle pinned,
/// 50% budget, 25%/day rate limit.
fun setup_external(scenario: &mut Scenario) {
    ts::next_tx(scenario, th::admin_addr());
    let cap = th::take_admin_cap(scenario);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    vault::set_external_account(
        &cap,
        &mut v,
        &oreg,
        external_addr(),
        type_name::with_defining_ids<TestOracle>(),
        5_000,
        2_500,
    );
    ts::return_shared(oreg);
    ts::return_shared(v);
    th::return_admin_cap(scenario, cap);
}

/// Deposit with the external equity leg recorded at `equity`.
fun deposit_with_equity(
    scenario: &mut Scenario,
    who: address,
    amount: u64,
    equity: u64,
    clock: &Clock,
) {
    ts::next_tx(scenario, who);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = th::take_protocol_config(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    vault::record_external_equity<TestOracle>(&v, &oreg, &mut appraisal, th::test_oracle(), equity);
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(th::mint<USDC>(amount), scenario.ctx()),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Curator releases `amount` to the external account, appraising with the
/// equity leg at `equity`.
fun release(scenario: &mut Scenario, amount: u64, equity: u64, clock: &Clock) {
    ts::next_tx(scenario, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    vault::record_external_equity<TestOracle>(&v, &oreg, &mut appraisal, th::test_oracle(), equity);
    vault::release_external<USDC>(&mut v, &cap, appraisal, amount, clock, scenario.ctx());
    ts::return_to_sender(scenario, cap);
    ts::return_shared(oreg);
    ts::return_shared(v);
}

#[test]
fun set_release_return_lifecycle() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    // Deposit first: once the account is registered, every appraisal
    // (including deposits) needs the equity leg.
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);

    ts::next_tx(&mut scenario, th::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::has_external_account(&v), 0);
    assert!(vault::external_account(&v) == external_addr(), 0);
    assert!(vault::external_exposure(&v) == 0, 0);
    ts::return_shared(v);

    // Budget 50% of NAV(1000) = 500: release 250.
    release(&mut scenario, 250, 0, &clock);

    ts::next_tx(&mut scenario, th::curator_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::external_exposure(&v) == 250, 0);
    assert!(vault::free_balance_of<USDC>(&v) == 750, 0);
    ts::return_shared(v);

    // The released coin landed at the external address; send part back.
    ts::next_tx(&mut scenario, external_addr());
    let mut received = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(received.value() == 250, 0);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let back = coin::split(&mut received, 100, scenario.ctx());
    vault::return_external<USDC>(&mut v, back, scenario.ctx());
    assert!(vault::external_exposure(&v) == 150, 0);
    assert!(vault::free_balance_of<USDC>(&v) == 850, 0);
    ts::return_shared(v);
    ts::return_to_sender(&scenario, received);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun equity_leg_prices_deposits_at_true_nav() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    // Alice deposits into the plain vault, THEN the external account is
    // registered and capital deployed.
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);

    // The venue made money: equity 300 vs 250 released → NAV 750+300.
    deposit_with_equity(&mut scenario, th::bob_addr(), 1_050, 300, &clock);

    ts::next_tx(&mut scenario, th::bob_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // Bob paid exactly one NAV: shares = 1050 × 1000 / 1050 = 1000.
    let (bob_shares, _, _) = vault::stake_of(&v, th::bob_addr());
    assert!(bob_shares == 1_000, 0);
    assert!(vault::total_shares(&v) == 2_000, 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 82, location = trading_vault::vault)] // appraisal_incomplete
fun appraisal_without_equity_leg_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    setup_external(&mut scenario);
    // simple_deposit appraises without the equity leg → incomplete.
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 104, location = trading_vault::vault)] // wrong_external_oracle
fun equity_from_unpinned_witness_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    setup_external(&mut scenario);

    ts::next_tx(&mut scenario, th::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    vault::record_external_equity<RogueOracle>(
        &v, &oreg, &mut appraisal, th::rogue_oracle(), 100,
    );
    abort 999
}

#[test]
#[expected_failure(abort_code = 76, location = trading_vault::vault)] // oracle_not_allowed
fun set_external_account_unallowlisted_oracle_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    vault::set_external_account(
        &cap,
        &mut v,
        &oreg,
        external_addr(),
        type_name::with_defining_ids<RogueOracle>(),
        5_000,
        2_500,
    );
    abort 999
}

#[test]
#[expected_failure(abort_code = 101, location = trading_vault::vault)] // external_budget_exceeded
fun release_beyond_budget_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    // Budget 50% of 1000 = 500.
    release(&mut scenario, 501, 0, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 102, location = trading_vault::vault)] // external_rate_limited
fun release_beyond_daily_window_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    // Daily 25% of NAV. First release: 25% of 1000 = 250 exactly — fine.
    release(&mut scenario, 250, 0, &clock);
    // Second in the same window: NAV still 1000 (750 cash + 250 equity),
    // window already holds 250 → even 1 more unit trips the limit.
    release(&mut scenario, 1, 250, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun release_window_resets_after_a_day() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);

    clock.set_for_testing(DAY_MS + 1);
    release(&mut scenario, 200, 250, &clock);

    ts::next_tx(&mut scenario, th::curator_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::external_exposure(&v) == 450, 0);
    let (_, _, in_window, window_start) = vault::external_limits(&v);
    assert!(in_window == 200 && window_start == DAY_MS + 1, 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 89, location = trading_vault::vault)] // not_authorized
fun return_from_foreign_sender_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);

    ts::next_tx(&mut scenario, th::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::return_external<USDC>(
        &mut v,
        coin::from_balance(th::mint<USDC>(100), scenario.ctx()),
        scenario.ctx(),
    );
    abort 999
}

#[test]
#[expected_failure(abort_code = 103, location = trading_vault::vault)] // external_exposure_open
fun clear_with_live_exposure_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::clear_external_account(&cap, &mut v);
    abort 999
}

#[test]
fun clear_after_full_return_restores_plain_appraisals() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);

    ts::next_tx(&mut scenario, external_addr());
    let received = ts::take_from_sender<Coin<USDC>>(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::return_external<USDC>(&mut v, received, scenario.ctx());
    assert!(vault::external_exposure(&v) == 0, 0);
    ts::return_shared(v);

    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    vault::clear_external_account(&cap, &mut v);
    assert!(!vault::has_external_account(&v), 0);
    ts::return_shared(v);
    th::return_admin_cap(&scenario, cap);

    // No equity leg needed anymore.
    th::simple_deposit(&mut scenario, th::bob_addr(), 500, &clock);

    clock.destroy_for_testing();
    ts::end(scenario);
}
