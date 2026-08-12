#[test_only]
module options_vault::test_helpers;

use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket;
use options_core::treasury::{Self, Treasury};

public struct USDC has drop {}
public struct BTC has drop {}
public struct CALL has drop {}
public struct CALL2 has drop {}

public fun admin_addr(): address { @0xA1 }
public fun writer_addr(): address { @0xB2 }
public fun trader_mm_addr(): address { @0xC3 }
public fun trader_addr(): address { @0xD4 }
public fun stranger_addr(): address { @0xF6 }

/// Initialize protocol: AdminCap to admin_addr, ProtocolConfig shared,
/// Treasury shared. Returns a fresh test Clock. (Local copy of the core
/// package's test helper — `#[test_only]` code isn't importable across
/// packages.)
public fun init_protocol(scenario: &mut Scenario): Clock {
    ts::next_tx(scenario, admin_addr());
    admin::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, admin_addr());
    let admin_cap = ts::take_from_sender<AdminCap>(scenario);
    treasury::create_and_share(&admin_cap, scenario.ctx());
    ts::return_to_sender(scenario, admin_cap);

    ts::next_tx(scenario, admin_addr());
    clock::create_for_testing(scenario.ctx())
}

public fun new_bucket<U, S, C>(
    scenario: &mut Scenario,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    ts::next_tx(scenario, admin_addr());
    let cap = take_admin_cap(scenario);
    let tcap = coin::create_treasury_cap_for_testing<C>(scenario.ctx());
    bucket::create_bucket<U, S, C>(&cap, tcap, expiry_ms, strike, strike_scale, scenario.ctx());
    return_admin_cap(scenario, cap);
}

public fun take_admin_cap(scenario: &Scenario): AdminCap {
    ts::take_from_address<AdminCap>(scenario, admin_addr())
}

public fun return_admin_cap(scenario: &Scenario, cap: AdminCap) {
    ts::return_to_address(admin_addr(), cap);
    let _ = scenario;
}

public fun take_config(scenario: &Scenario): ProtocolConfig {
    ts::take_shared<ProtocolConfig>(scenario)
}

public fun take_treasury(scenario: &Scenario): Treasury {
    ts::take_shared<Treasury>(scenario)
}
