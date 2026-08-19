/// External-account primitives: registration, budgeted + rate-limited
/// releases, sender-gated returns, and the mandatory equity leg in every
/// appraisal.
///
/// v2 port notes: `release_external` aborts 124 while risk-off, so every
/// test that releases funds the curator commitment (100) right after
/// vault creation — NAV baselines shift from 1000 to 1100 (budget 50% →
/// 550, daily 25% → 275). Tests that never release stay commitment-free
/// and keep the v1 numbers.
#[test_only]
module vault_v2::external_account_tests;

use std::type_name;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use whitelist::whitelist::Whitelist;
use options_core::treasury::Treasury;

use vault_v2::test_helpers::{Self as th, USDC, TestOracle, RogueOracle};
use vault_v2::registry::OracleRegistry;
use vault_v2::vault::{Self, CuratorCap, TradingVault};
use vault_v2::vault_position::{Self, VaultPosition};

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

/// Deposit with the external equity leg recorded at `equity`; the minted
/// position transfers to `who`.
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
    let wl = ts::take_shared<Whitelist>(scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    vault::record_external_equity<TestOracle>(&v, &oreg, &mut appraisal, th::test_oracle(), equity);
    let position = vault::deposit<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        coin::from_balance(th::mint<USDC>(amount), scenario.ctx()),
        option::none(),
        th::untranched(),
        clock,
        scenario.ctx(),
    );
    transfer::public_transfer(position, who);
    ts::return_shared(wl);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Curator releases `amount` to the external account, appraising with the
/// equity leg at `equity` — attached only when exposure is live, the same
/// gate off-chain composers apply.
fun release(scenario: &mut Scenario, amount: u64, equity: u64, clock: &Clock) {
    ts::next_tx(scenario, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = th::take_protocol_config(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    if (vault::external_exposure(&v) > 0) {
        vault::record_external_equity<TestOracle>(
            &v, &oreg, &mut appraisal, th::test_oracle(), equity,
        );
    };
    let _nav = vault::release_external<USDC>(
        &mut v, &cap, &cfg, appraisal, amount, clock, scenario.ctx(),
    );
    ts::return_to_sender(scenario, cap);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

#[test]
fun set_release_return_lifecycle() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
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

    // Budget 50% of NAV(1100) = 550: release 250.
    release(&mut scenario, 250, 0, &clock);

    ts::next_tx(&mut scenario, th::curator_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::external_exposure(&v) == 250, 0);
    assert!(vault::free_balance_of<USDC>(&v) == 850, 0);
    ts::return_shared(v);

    // The released coin landed at the external address; send part back.
    ts::next_tx(&mut scenario, external_addr());
    let mut received = ts::take_from_sender<Coin<USDC>>(&scenario);
    assert!(received.value() == 250, 0);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let back = coin::split(&mut received, 100, scenario.ctx());
    vault::return_external<USDC>(&mut v, back, scenario.ctx());
    assert!(vault::external_exposure(&v) == 150, 0);
    assert!(vault::free_balance_of<USDC>(&v) == 950, 0);
    ts::return_shared(v);
    ts::return_to_sender(&scenario, received);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun equity_leg_prices_deposits_at_true_nav() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
    // Alice deposits into the plain vault, THEN the external account is
    // registered and capital deployed.
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);

    // Snapshot supply before Bob's entry.
    ts::next_tx(&mut scenario, th::admin_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let supply = vault::total_shares(&v);
    ts::return_shared(v);

    // The venue made money: equity 300 vs 250 released → NAV 850+300.
    deposit_with_equity(&mut scenario, th::bob_addr(), 1_050, 300, &clock);

    ts::next_tx(&mut scenario, th::bob_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    // Bob paid one NAV (offset dust aside): 1050 valued against NAV 1150.
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    let expected = th::expected_shares(1_050, supply, 1_150);
    assert!(vault_position::shares(&p) == expected, 0);
    assert!(vault::total_shares(&v) == supply + expected, 0);
    ts::return_to_sender(&scenario, p);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 82, location = vault_v2::vault)] // appraisal_incomplete
fun appraisal_without_equity_leg_aborts_once_funded() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    release(&mut scenario, 250, 0, &clock);
    // Exposure is live now, so simple_deposit's leg-less appraisal is
    // incomplete — the requirement re-engages exactly when funds are out.
    th::simple_deposit(&mut scenario, th::bob_addr(), 1_000, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun unfunded_external_vault_deposits_and_exits_without_equity_leg() {
    // SO-310: registering an account must not put an equity poster on the
    // critical path of user deposits/exits. Nothing has been released, so
    // the account's equity is zero by construction.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    setup_external(&mut scenario);

    // Deposit into a registered-but-unfunded vault: no leg, no oracle.
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);

    ts::next_tx(&mut scenario, th::alice_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let p = ts::take_from_sender<VaultPosition>(&scenario);
    assert!(vault_position::shares(&p) == 1_000_000_000, 0);
    clock.set_for_testing(4_000_000); // past lockup
    vault::request_withdraw<USDC>(&mut v, p, &clock, scenario.ctx());
    ts::return_shared(v);

    // Permissionless fulfillment, still leg-less.
    ts::next_tx(&mut scenario, th::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let mut treasury = ts::take_shared<Treasury>(&scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::fulfill_withdrawals<USDC>(&mut v, &cfg, &mut treasury, appraisal, &clock, scenario.ctx());
    assert!(vault::pending_withdrawals(&v) == 0, 0);
    assert!(vault::total_shares(&v) == 0, 0);
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);

    ts::next_tx(&mut scenario, th::alice_addr());
    let paid = ts::take_from_address<Coin<USDC>>(&scenario, th::alice_addr());
    assert!(paid.value() == 1_000, 0);
    ts::return_to_address(th::alice_addr(), paid);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 87, location = vault_v2::vault)] // already_appraised
fun equity_leg_on_unfunded_vault_aborts() {
    // The leg is not merely optional at zero exposure — it is rejected,
    // so an attested number can never be added on top of the vault's own
    // by-construction zero.
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);

    ts::next_tx(&mut scenario, th::alice_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    vault::record_external_equity<TestOracle>(&v, &oreg, &mut appraisal, th::test_oracle(), 100);
    abort 999
}

#[test]
#[expected_failure(abort_code = 104, location = vault_v2::vault)] // wrong_external_oracle
fun equity_from_unpinned_witness_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
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
#[expected_failure(abort_code = 76, location = vault_v2::vault)] // oracle_not_allowed
fun set_external_account_unallowlisted_oracle_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);

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
#[expected_failure(abort_code = 101, location = vault_v2::vault)] // external_budget_exceeded
fun release_beyond_budget_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    // Budget 50% of 1100 = 550.
    release(&mut scenario, 551, 0, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 102, location = vault_v2::vault)] // external_rate_limited
fun release_beyond_daily_window_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
    th::simple_deposit(&mut scenario, th::alice_addr(), 1_000, &clock);
    setup_external(&mut scenario);
    // Daily 25% of NAV. First release: 25% of 1100 = 275 exactly — fine.
    release(&mut scenario, 275, 0, &clock);
    // Second in the same window: NAV still 1100 (825 cash + 275 equity),
    // window already holds 275 → even 1 more unit trips the limit.
    release(&mut scenario, 1, 275, &clock);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
fun release_window_resets_after_a_day() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
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
#[expected_failure(abort_code = 89, location = vault_v2::vault)] // not_authorized
fun return_from_foreign_sender_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
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
#[expected_failure(abort_code = 103, location = vault_v2::vault)] // external_exposure_open
fun clear_with_live_exposure_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
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
    th::new_default_vault(&mut scenario, &clock);
    th::fund_commitment(&mut scenario, 100, &clock);
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
