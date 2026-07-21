#[test_only]
module equity_oracle::equity_oracle_tests;

use std::type_name;
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use trading_vault::registry::{Self as tv_registry, OracleRegistry};
use trading_vault::test_helpers::{Self as th, USDC};
use trading_vault::vault::{Self, CuratorCap, TradingVault};

use equity_oracle::equity_oracle::{Self as eo, EquityBook, EquityOracle};

const DAY_MS: u64 = 86_400_000;

fun external_addr(): address { @0xF00D }

fun keeper_addr(): address { th::bob_addr() }

/// Protocol + book + vault with the external account pinned to the
/// `EquityOracle` witness (budget 50%, daily 25%), keeper allowlisted as
/// poster, and alice funding 1000 USDC before registration.
fun setup(scenario: &mut Scenario): Clock {
    let clock = th::init_protocol(scenario);

    ts::next_tx(scenario, th::admin_addr());
    eo::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut oreg = ts::take_shared<OracleRegistry>(scenario);
    tv_registry::allow_oracle(
        &admin_cap,
        &mut oreg,
        type_name::with_defining_ids<EquityOracle>(),
    );
    ts::return_shared(oreg);
    let mut book = ts::take_shared<EquityBook>(scenario);
    eo::add_poster(&admin_cap, &mut book, keeper_addr());
    ts::return_shared(book);
    th::return_admin_cap(scenario, admin_cap);

    th::new_default_vault(scenario);
    th::simple_deposit(scenario, th::alice_addr(), 1_000, &clock);

    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    vault::set_external_account(
        &admin_cap,
        &mut v,
        &oreg,
        external_addr(),
        type_name::with_defining_ids<EquityOracle>(),
        5_000,
        2_500,
    );
    ts::return_shared(oreg);
    ts::return_shared(v);
    th::return_admin_cap(scenario, admin_cap);
    clock
}

fun seed(scenario: &mut Scenario, equity: u64, clock: &Clock) {
    ts::next_tx(scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(scenario);
    let mut book = ts::take_shared<EquityBook>(scenario);
    let v = ts::take_shared<TradingVault>(scenario);
    eo::seed_equity(&admin_cap, &mut book, object::id(&v), equity, clock, scenario.ctx());
    ts::return_shared(v);
    ts::return_shared(book);
    th::return_admin_cap(scenario, admin_cap);
}

fun post(scenario: &mut Scenario, who: address, equity: u64, clock: &Clock) {
    ts::next_tx(scenario, who);
    let mut book = ts::take_shared<EquityBook>(scenario);
    let v = ts::take_shared<TradingVault>(scenario);
    eo::post_equity(&mut book, object::id(&v), equity, clock, scenario.ctx());
    ts::return_shared(v);
    ts::return_shared(book);
}

/// Curator release with the equity leg recorded from the book.
fun release(scenario: &mut Scenario, amount: u64, clock: &Clock) {
    ts::next_tx(scenario, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let book = ts::take_shared<EquityBook>(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    eo::record(&v, &book, &oreg, &mut appraisal, clock);
    vault::release_external<USDC>(&mut v, &cap, appraisal, amount, clock, scenario.ctx());
    ts::return_to_sender(scenario, cap);
    ts::return_shared(oreg);
    ts::return_shared(book);
    ts::return_shared(v);
}

#[test]
fun post_within_guardrails_and_appraise() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);

    seed(&mut scenario, 0, &clock);
    release(&mut scenario, 250, &clock);
    // The keeper marks the account at cost, then at a profit within the
    // 20% delta band after the 1-minute interval.
    clock.set_for_testing(60_000);
    // 0 → 250 would be a bps-of-zero move: only the admin can anchor it.
    seed(&mut scenario, 250, &clock);
    clock.set_for_testing(120_000);
    post(&mut scenario, keeper_addr(), 290, &clock);

    ts::next_tx(&mut scenario, th::admin_addr());
    let book = ts::take_shared<EquityBook>(&scenario);
    let v = ts::take_shared<TradingVault>(&scenario);
    let (equity, ts_ms) = eo::entry(&book, object::id(&v));
    assert!(equity == 290 && ts_ms == 120_000, 0);
    ts::return_shared(v);
    ts::return_shared(book);

    // Bob deposits at true NAV: 750 cash + 290 equity = 1040.
    ts::next_tx(&mut scenario, th::bob_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let book = ts::take_shared<EquityBook>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let mut appraisal = vault::begin_appraisal<USDC>(&v);
    eo::record(&v, &book, &oreg, &mut appraisal, &clock);
    assert!(vault::appraisal_value(&appraisal) == 1_040, 0);
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        appraisal,
        coin::from_balance(th::mint<USDC>(1_040), scenario.ctx()),
        &clock,
        scenario.ctx(),
    );
    let (bob_shares, _, _) = vault::stake_of(&v, th::bob_addr());
    assert!(bob_shares == 1_000, 0);
    ts::return_shared(oreg);
    ts::return_shared(book);
    ts::return_shared(cfg);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 1, location = equity_oracle::equity_oracle)] // E_NOT_POSTER
fun post_by_stranger_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 250, &clock);
    clock.set_for_testing(60_000);
    post(&mut scenario, th::alice_addr(), 260, &clock);
    abort 999
}

#[test]
#[expected_failure(abort_code = 2, location = equity_oracle::equity_oracle)] // E_NOT_SEEDED
fun post_unseeded_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    post(&mut scenario, keeper_addr(), 260, &clock);
    abort 999
}

#[test]
#[expected_failure(abort_code = 3, location = equity_oracle::equity_oracle)] // E_TOO_SOON
fun post_inside_min_interval_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 250, &clock);
    clock.set_for_testing(59_999);
    post(&mut scenario, keeper_addr(), 260, &clock);
    abort 999
}

#[test]
#[expected_failure(abort_code = 4, location = equity_oracle::equity_oracle)] // E_DELTA_TOO_LARGE
fun post_beyond_delta_band_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 250, &clock);
    clock.set_for_testing(60_000);
    // 20% of 250 = 50 → 301 is one unit past the band.
    post(&mut scenario, keeper_addr(), 301, &clock);
    abort 999
}

#[test]
fun post_at_delta_band_edge_ok() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 250, &clock);
    clock.set_for_testing(60_000);
    post(&mut scenario, keeper_addr(), 300, &clock);
    clock.set_for_testing(120_000);
    post(&mut scenario, keeper_addr(), 240, &clock); // −20% band
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 5, location = equity_oracle::equity_oracle)] // E_STALE
fun record_stale_entry_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 0, &clock);
    // Entry from t=0; the book's 5-minute backstop lapses.
    clock.set_for_testing(300_001);
    release(&mut scenario, 100, &clock);
    abort 999
}

#[test]
#[expected_failure(abort_code = 2, location = equity_oracle::equity_oracle)] // E_NOT_SEEDED
fun record_unseeded_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = setup(&mut scenario);
    release(&mut scenario, 100, &clock);
    abort 999
}

#[test]
fun window_reset_and_budget_track_nav_with_equity() {
    // Releases across two windows, with the second window's limits
    // computed against a NAV that includes venue P&L.
    let mut scenario = ts::begin(th::admin_addr());
    let mut clock = setup(&mut scenario);
    seed(&mut scenario, 0, &clock);
    release(&mut scenario, 250, &clock); // NAV 1000, daily cap 250

    clock.set_for_testing(DAY_MS + 1);
    seed(&mut scenario, 400, &clock); // venue profit: NAV = 750 + 400
    release(&mut scenario, 150, &clock); // budget 50% of 1150 = 575 ≥ 400

    ts::next_tx(&mut scenario, th::curator_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::external_exposure(&v) == 400, 0);
    assert!(vault::free_balance_of<USDC>(&v) == 600, 0);
    ts::return_shared(v);

    clock.destroy_for_testing();
    ts::end(scenario);
}
