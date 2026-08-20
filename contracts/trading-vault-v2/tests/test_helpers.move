#[test_only]
module vault_v2::test_helpers;

use std::type_name;
use sui::balance::{Self, Balance};
use sui::clock::{Self, Clock};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap};
use options_core::treasury::{Self, Treasury};
use whitelist::whitelist::{Self, AdminCap as WlAdminCap, Whitelist};

use vault_v2::price::{Self, PriceAttestation};
use vault_v2::registry::{Self, IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use vault_v2::vault::{Self, CuratorCap, TradingVault};
use vault_v2::vault_position::VaultPosition;

public struct USDC has drop {}
public struct BTC has drop {}

/// Allowlisted test integration adapter witness.
public struct TestAdapter has drop {}

/// A second adapter, for cross-adapter authorization tests.
public struct OtherAdapter has drop {}

/// Allowlisted test oracle witness.
public struct TestOracle has drop {}

/// An oracle witness that never gets allowlisted.
public struct RogueOracle has drop {}

/// Position object for custody tests.
public struct TestPosition has key, store {
    id: UID,
}

public fun admin_addr(): address { @0xA1 }
public fun curator_addr(): address { @0xC3 }
public fun alice_addr(): address { @0xD4 }
public fun bob_addr(): address { @0xE5 }

public fun test_adapter(): TestAdapter { TestAdapter {} }

public fun other_adapter(): OtherAdapter { OtherAdapter {} }

public fun rogue_oracle(): RogueOracle { RogueOracle {} }

public fun test_oracle(): TestOracle { TestOracle {} }

public fun untranched(): u8 { 0 }

public fun senior(): u8 { 1 }

public fun junior(): u8 { 2 }

public fun new_position(scenario: &mut Scenario): TestPosition {
    TestPosition { id: object::new(scenario.ctx()) }
}

public fun destroy_position(p: TestPosition) {
    let TestPosition { id } = p;
    id.delete();
}

/// Initialize the protocol: options_core admin + treasury, vault_v2
/// registries, TestAdapter + TestOracle allowlisted, all named actors
/// whitelisted. Returns a test Clock at t=0.
public fun init_protocol(scenario: &mut Scenario): Clock {
    ts::next_tx(scenario, admin_addr());
    admin::init_for_testing(scenario.ctx());
    whitelist::init_for_testing(scenario.ctx());
    registry::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, admin_addr());
    let admin_cap = ts::take_from_sender<AdminCap>(scenario);
    treasury::create_and_share(&admin_cap, scenario.ctx());
    let mut ireg = ts::take_shared<IntegrationRegistry>(scenario);
    registry::allow_adapter(&admin_cap, &mut ireg, type_name::with_defining_ids<TestAdapter>());
    ts::return_shared(ireg);
    let mut oreg = ts::take_shared<OracleRegistry>(scenario);
    registry::allow_oracle(&admin_cap, &mut oreg, type_name::with_defining_ids<TestOracle>());
    ts::return_shared(oreg);
    let wl_cap = ts::take_from_sender<WlAdminCap>(scenario);
    let mut wl = ts::take_shared<Whitelist>(scenario);
    whitelist::add_member_for_testing(&mut wl, admin_addr());
    whitelist::add_member_for_testing(&mut wl, curator_addr());
    whitelist::add_member_for_testing(&mut wl, alice_addr());
    whitelist::add_member_for_testing(&mut wl, bob_addr());
    ts::return_shared(wl);
    ts::return_to_sender(scenario, wl_cap);
    ts::return_to_sender(scenario, admin_cap);

    ts::next_tx(scenario, admin_addr());
    clock::create_for_testing(scenario.ctx())
}

/// Create an UNTRANCHED USDC vault as curator_addr. Defaults: 1000 bps
/// curator fee, 1h lockup, 1h grace.
public fun new_default_vault(scenario: &mut Scenario, clock: &Clock): ID {
    ts::next_tx(scenario, curator_addr());
    let cfg = take_protocol_config(scenario);
    let wl = take_whitelist(scenario);
    let id = vault::create_vault<USDC>(
        &cfg,
        &wl,
        3_600_000, // lockup
        1_000, // curator fee bps
        3_600_000, // unwind grace
        0, // structure: untranched
        0,
        0,
        0,
        0,
        0,
        0,
        1, // terms_version
        b"spec-hash-test",
        clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    ts::return_shared(cfg);
    id
}

/// Create a TRANCHED USDC vault as curator_addr. Defaults: 10% annual
/// hurdle, 20% target / 10% maintenance junior buffer, PreferredOnly
/// upside; 1000 bps curator fee, 1h lockup, 1h grace.
public fun new_tranched_vault(scenario: &mut Scenario, clock: &Clock): ID {
    new_tranched_vault_with_upside(scenario, clock, 0, 0, 0)
}

public fun new_tranched_vault_with_upside(
    scenario: &mut Scenario,
    clock: &Clock,
    upside_code: u8,
    participation_bps: u64,
    total_return_cap_bps: u64,
): ID {
    ts::next_tx(scenario, curator_addr());
    let cfg = take_protocol_config(scenario);
    let wl = take_whitelist(scenario);
    let id = vault::create_vault<USDC>(
        &cfg,
        &wl,
        3_600_000,
        1_000,
        3_600_000,
        1, // structure: senior/junior
        1_000, // 10% annual hurdle
        2_000, // 20% target junior
        1_000, // 10% maintenance junior
        upside_code,
        participation_bps,
        total_return_cap_bps,
        1,
        b"spec-hash-test",
        clock,
        scenario.ctx(),
    );
    ts::return_shared(wl);
    ts::return_shared(cfg);
    id
}

public fun mint<T>(amount: u64): Balance<T> {
    balance::create_for_testing<T>(amount)
}

/// Deposit `amount` USDC as `who` into tranche `tranche_code` of a
/// USDC-only vault; the minted position is transferred to `who`.
public fun deposit_usdc(
    scenario: &mut Scenario,
    who: address,
    amount: u64,
    tranche_code: u8,
    clock: &Clock,
) {
    ts::next_tx(scenario, who);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = take_protocol_config(scenario);
    let wl = take_whitelist(scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    let position = vault::deposit<USDC>(
        &mut v,
        &cfg,
        &wl,
        appraisal,
        sui::coin::from_balance(mint<USDC>(amount), scenario.ctx()),
        option::none(),
        tranche_code,
        clock,
        scenario.ctx(),
    );
    transfer::public_transfer(position, who);
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Untranched convenience.
public fun simple_deposit(scenario: &mut Scenario, who: address, amount: u64, clock: &Clock) {
    deposit_usdc(scenario, who, amount, untranched(), clock)
}

/// Curator funds the escrowed commitment position with `amount` USDC.
public fun fund_commitment(scenario: &mut Scenario, amount: u64, clock: &Clock) {
    ts::next_tx(scenario, curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = take_protocol_config(scenario);
    let wl = take_whitelist(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::deposit_into_commitment<USDC>(
        &mut v,
        &cfg,
        &wl,
        &cap,
        appraisal,
        sui::coin::from_balance(mint<USDC>(amount), scenario.ctx()),
        option::none(),
        clock,
        scenario.ctx(),
    );
    ts::return_to_sender(scenario, cap);
    ts::return_shared(wl);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Queue a withdrawal of `who`'s wallet position (whole object), payable
/// in USDC.
public fun request_withdraw_all(scenario: &mut Scenario, who: address, clock: &Clock) {
    ts::next_tx(scenario, who);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let position = ts::take_from_sender<VaultPosition>(scenario);
    vault::request_withdraw<USDC>(&mut v, position, clock, scenario.ctx());
    ts::return_shared(v);
}

/// Run the all-USDC fulfillment crank as admin.
public fun run_fulfillment(scenario: &mut Scenario, clock: &Clock) {
    ts::next_tx(scenario, admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = take_protocol_config(scenario);
    let mut treasury = ts::take_shared<Treasury>(scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::fulfill_withdrawals<USDC>(&mut v, &cfg, &mut treasury, appraisal, clock, scenario.ctx());
    ts::return_shared(treasury);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Permissionless capital crank (accrual + risk-state sync).
public fun crank_capital(scenario: &mut Scenario, clock: &Clock) {
    ts::next_tx(scenario, admin_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = take_protocol_config(scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::crank_capital(&mut v, &cfg, appraisal, clock);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Offset-adjusted share mint for `value` accounting units at
/// (supply, nav).
public fun expected_shares(value: u64, supply: u128, nav: u128): u128 {
    (
        ((value as u256) * ((supply + vault::share_offset()) as u256))
            / ((nav + 1) as u256),
    ) as u128
}

/// Offset-adjusted crystallization value of `shares` at (supply, nav).
public fun expected_value(shares: u128, supply: u128, nav: u128): u64 {
    (
        ((shares as u256) * ((nav + 1) as u256))
            / ((supply + vault::share_offset()) as u256),
    ) as u64
}

/// Simulate strategy P&L: a curator session that returns `amount` of `T`
/// to the vault (profit) without taking anything. Works in risk-off
/// states too (put-only).
public fun session_gain<T>(scenario: &mut Scenario, amount: u64) {
    ts::next_tx(scenario, curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let ireg = ts::take_shared<IntegrationRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let mut s = vault::begin_session(&v, &cap, &ireg, TestAdapter {});
    vault::put<T>(&mut v, &mut s, mint<T>(amount));
    vault::end_session(&v, s);
    ts::return_to_sender(scenario, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
}

/// Simulate strategy loss: a curator session that takes `amount` USDC
/// and burns it. Requires risk-on (take-capable session).
public fun session_loss(scenario: &mut Scenario, amount: u64) {
    ts::next_tx(scenario, curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let ireg = ts::take_shared<IntegrationRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    let mut s = vault::begin_session(&v, &cap, &ireg, TestAdapter {});
    let lost = vault::take<USDC>(&mut v, &mut s, amount);
    balance::destroy_for_testing(lost);
    vault::end_session(&v, s);
    ts::return_to_sender(scenario, cap);
    ts::return_shared(ireg);
    ts::return_shared(v);
}

/// Mint a TestOracle attestation for Asset→Quote at `price` (scale 1e12).
public fun attest<Asset, Quote>(
    scenario: &Scenario,
    price_scaled: u128,
    timestamp_ms: u64,
): PriceAttestation {
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let att = price::attest(
        TestOracle {},
        &oreg,
        type_name::with_defining_ids<Asset>(),
        type_name::with_defining_ids<Quote>(),
        price_scaled,
        timestamp_ms,
    );
    ts::return_shared(oreg);
    att
}

public fun take_protocol_config(scenario: &Scenario): VaultProtocolConfig {
    ts::take_shared<VaultProtocolConfig>(scenario)
}

public fun take_whitelist(scenario: &Scenario): Whitelist {
    ts::take_shared<Whitelist>(scenario)
}

public fun take_wl_admin_cap(scenario: &Scenario): WlAdminCap {
    ts::take_from_address<WlAdminCap>(scenario, admin_addr())
}

public fun return_wl_admin_cap(scenario: &Scenario, cap: WlAdminCap) {
    ts::return_to_address(admin_addr(), cap);
    let _ = scenario;
}

public fun take_admin_cap(scenario: &Scenario): AdminCap {
    ts::take_from_address<AdminCap>(scenario, admin_addr())
}

public fun return_admin_cap(scenario: &Scenario, cap: AdminCap) {
    ts::return_to_address(admin_addr(), cap);
    let _ = scenario;
}
