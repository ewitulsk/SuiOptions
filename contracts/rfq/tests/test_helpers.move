#[test_only]
module options_rfq::test_helpers;

use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket;
use options_core::put_bucket;
use options_core::treasury::{Self, Treasury};

public struct USDC has drop {}
public struct BTC has drop {}
public struct CALL has drop {}
public struct PUT has drop {}

public fun admin_addr(): address { @0xA1 }
public fun seller_addr(): address { @0xB2 }
public fun bidder_a(): address { @0xC3 }
public fun bidder_b(): address { @0xE5 }

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
    let cap = ts::take_from_address<AdminCap>(scenario, admin_addr());
    let tcap = coin::create_treasury_cap_for_testing<C>(scenario.ctx());
    bucket::create_bucket<U, S, C>(&cap, tcap, expiry_ms, strike, strike_scale, scenario.ctx());
    ts::return_to_address(admin_addr(), cap);
}

public fun new_put_bucket<U, S, P>(
    scenario: &mut Scenario,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    ts::next_tx(scenario, admin_addr());
    let cap = ts::take_from_address<AdminCap>(scenario, admin_addr());
    let tcap = coin::create_treasury_cap_for_testing<P>(scenario.ctx());
    put_bucket::create_put_bucket<U, S, P>(
        &cap, tcap, expiry_ms, strike, strike_scale, scenario.ctx(),
    );
    ts::return_to_address(admin_addr(), cap);
}

public fun take_config(scenario: &Scenario): ProtocolConfig {
    ts::take_shared<ProtocolConfig>(scenario)
}

public fun take_treasury(scenario: &Scenario): Treasury {
    ts::take_shared<Treasury>(scenario)
}
