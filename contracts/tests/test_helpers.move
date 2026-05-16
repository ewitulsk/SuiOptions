#[test_only]
module options_protocol::test_helpers;

use sui::clock::{Self, Clock};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::account::{Self, Account};
use options_protocol::treasury::{Self, Treasury};

public struct USDC has drop {}
public struct BTC has drop {}

public fun admin_addr(): address { @0xA1 }
public fun writer_addr(): address { @0xB2 }
public fun trader_mm_addr(): address { @0xC3 }
public fun trader_addr(): address { @0xD4 }
public fun writer_mm_addr(): address { @0xE5 }
public fun stranger_addr(): address { @0xF6 }

public fun pubkey_a(): vector<u8> {
    x"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
}

public fun pubkey_b(): vector<u8> {
    x"3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
}

/// Initialize protocol: AdminCap to admin_addr, ProtocolConfig shared, Treasury shared.
/// Returns a fresh test Clock.
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

/// Create and share an Account owned by `owner` with the given signing pubkey.
public fun create_account(scenario: &mut Scenario, owner: address, pubkey: vector<u8>) {
    ts::next_tx(scenario, owner);
    account::create_and_share_account(pubkey, scenario.ctx());
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

public fun take_account(scenario: &Scenario): Account {
    ts::take_shared<Account>(scenario)
}

public fun take_account_by_id(scenario: &Scenario, id: ID): Account {
    ts::take_shared_by_id<Account>(scenario, id)
}
