#[test_only]
module trading_vault::test_helpers;

use std::type_name;
use sui::balance::{Self, Balance};
use sui::clock::{Self, Clock};
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap};
use options_core::treasury;

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{Self, IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, CuratorCap, TradingVault};

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
public fun creator_addr(): address { @0xB2 }
public fun curator_addr(): address { @0xC3 }
public fun alice_addr(): address { @0xD4 }
public fun bob_addr(): address { @0xE5 }

public fun test_adapter(): TestAdapter { TestAdapter {} }

public fun other_adapter(): OtherAdapter { OtherAdapter {} }

public fun rogue_oracle(): RogueOracle { RogueOracle {} }

public fun test_oracle(): TestOracle { TestOracle {} }

public fun new_position(scenario: &mut Scenario): TestPosition {
    TestPosition { id: object::new(scenario.ctx()) }
}

public fun destroy_position(p: TestPosition) {
    let TestPosition { id } = p;
    id.delete();
}

/// Initialize the protocol: options_core admin (AdminCap → admin) +
/// treasury, trading_vault registries, TestAdapter + TestOracle
/// allowlisted. Returns a test Clock at t=0.
public fun init_protocol(scenario: &mut Scenario): Clock {
    ts::next_tx(scenario, admin_addr());
    admin::init_for_testing(scenario.ctx());
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
    ts::return_to_sender(scenario, admin_cap);

    ts::next_tx(scenario, admin_addr());
    clock::create_for_testing(scenario.ctx())
}

/// Create a USDC vault as creator with curator = curator_addr. Defaults:
/// 1000 bps curator fee, ROTATE_EITHER, 8 positions, 1h lockup, 1h grace.
public fun new_default_vault(scenario: &mut Scenario): ID {
    ts::next_tx(scenario, creator_addr());
    let cfg = take_protocol_config(scenario);
    let id = vault::create_vault<USDC>(
        &cfg,
        curator_addr(),
        3_600_000, // lockup
        1_000, // curator fee bps
        2, // ROTATE_EITHER
        8, // max positions
        3_600_000, // unwind grace
        scenario.ctx(),
    );
    ts::return_shared(cfg);
    id
}

public fun mint<T>(amount: u64): Balance<T> {
    balance::create_for_testing<T>(amount)
}

/// Deposit `amount` USDC as `who` into an empty-or-USDC-only vault
/// (appraisal completes with no attestations).
public fun simple_deposit(scenario: &mut Scenario, who: address, amount: u64, clock: &Clock) {
    ts::next_tx(scenario, who);
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = take_protocol_config(scenario);
    let appraisal = vault::begin_appraisal<USDC>(&v);
    vault::deposit<USDC>(
        &mut v,
        &cfg,
        appraisal,
        sui::coin::from_balance(mint<USDC>(amount), scenario.ctx()),
        clock,
        scenario.ctx(),
    );
    ts::return_shared(cfg);
    ts::return_shared(v);
}

/// Simulate strategy P&L: a curator session that returns `amount` of `T`
/// to the vault (profit) without taking anything.
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
/// and burns it.
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

public fun take_admin_cap(scenario: &Scenario): AdminCap {
    ts::take_from_address<AdminCap>(scenario, admin_addr())
}

public fun return_admin_cap(scenario: &Scenario, cap: AdminCap) {
    ts::return_to_address(admin_addr(), cap);
    let _ = scenario;
}
