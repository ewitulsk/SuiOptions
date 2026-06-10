#[test_only]
module options_protocol::test_helpers;

use std::string;
use sui::clock::{Self, Clock};
use sui::coin::{Self, Coin};
use sui::test_scenario::{Self as ts, Scenario};

use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::account::{Self, Account};
use options_protocol::bucket::{Self, Bucket};
use options_protocol::org::{Self, Org, OrgCap};
use options_protocol::quote::SignedQuote;
use options_protocol::treasury::{Self, Treasury};

public struct USDC has drop {}
public struct BTC has drop {}

/// Per-bucket option-coin marker types. Each bucket needs a distinct `Call`
/// type for true isolation; tests that create more than one bucket use a
/// different marker per bucket. `create_treasury_cap_for_testing` lets us
/// forge a fresh, zero-supply cap for an arbitrary type without an OTW.
public struct CALL has drop {}
public struct CALL2 has drop {}
public struct CALL3 has drop {}

/// Create and share a bucket for `(U, S, C)` with a fresh test option-coin
/// treasury cap. Runs as the org owner with the test org's cap. Mirrors what
/// a scheduler does on chain (publish a coin package, then `create_bucket`
/// with the new cap), minus the publish step.
public fun new_bucket<U, S, C>(
    scenario: &mut Scenario,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    ts::next_tx(scenario, org_owner_addr());
    let cap = take_org_cap(scenario);
    let tcap = coin::create_treasury_cap_for_testing<C>(scenario.ctx());
    bucket::create_bucket<U, S, C>(&cap, tcap, expiry_ms, strike, strike_scale, scenario.ctx());
    return_org_cap(scenario, cap);
}

public fun admin_addr(): address { @0xA1 }
public fun writer_addr(): address { @0xB2 }
public fun trader_mm_addr(): address { @0xC3 }
public fun trader_addr(): address { @0xD4 }
public fun writer_mm_addr(): address { @0xE5 }
public fun stranger_addr(): address { @0xF6 }
public fun org_owner_addr(): address { @0x07 }

public fun pubkey_a(): vector<u8> {
    x"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
}

public fun pubkey_b(): vector<u8> {
    x"3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
}

/// Initialize protocol: AdminCap to admin_addr, ProtocolConfig + Treasury
/// shared, and a fee-0 test org (OrgCap to org_owner_addr). The zero org fee
/// keeps pre-org premium arithmetic in existing tests unchanged.
/// Returns a fresh test Clock.
public fun init_protocol(scenario: &mut Scenario): Clock {
    ts::next_tx(scenario, admin_addr());
    admin::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, admin_addr());
    let admin_cap = ts::take_from_sender<AdminCap>(scenario);
    treasury::create_and_share(&admin_cap, scenario.ctx());
    ts::return_to_sender(scenario, admin_cap);

    ts::next_tx(scenario, org_owner_addr());
    let org_cap = org::create_org(string::utf8(b"test-org"), 0, scenario.ctx());
    transfer::public_transfer(org_cap, org_owner_addr());

    ts::next_tx(scenario, admin_addr());
    clock::create_for_testing(scenario.ctx())
}

public fun scheme_ed25519(): u8 { 0 }
public fun scheme_secp256k1(): u8 { 1 }
public fun scheme_secp256r1(): u8 { 2 }

/// Create and share an Account owned by `owner` with an Ed25519 signing key.
/// Default for existing tests — see `create_account_with_scheme` for the
/// per-scheme variant.
public fun create_account(scenario: &mut Scenario, owner: address, pubkey: vector<u8>) {
    create_account_with_scheme(scenario, owner, scheme_ed25519(), pubkey)
}

/// Create and share an Account using an explicit signing scheme byte.
public fun create_account_with_scheme(
    scenario: &mut Scenario,
    owner: address,
    scheme: u8,
    pubkey: vector<u8>,
) {
    ts::next_tx(scenario, owner);
    account::create_and_share_account(scheme, pubkey, scenario.ctx());
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

public fun take_org(scenario: &Scenario): Org {
    ts::take_shared<Org>(scenario)
}

public fun take_org_by_id(scenario: &Scenario, id: ID): Org {
    ts::take_shared_by_id<Org>(scenario, id)
}

public fun take_org_cap(scenario: &Scenario): OrgCap {
    ts::take_from_address<OrgCap>(scenario, org_owner_addr())
}

public fun return_org_cap(scenario: &Scenario, cap: OrgCap) {
    ts::return_to_address(org_owner_addr(), cap);
    let _ = scenario;
}

/// Writer-flow wrapper preserving the pre-refactor test surface: forwards
/// the returned `(Position, Coin<Settlement>)` to `recipient`, so existing
/// assertions via `take_from_address` keep working.
public fun execute_writer_flow_for_testing<U, S, C>(
    scenario: &mut Scenario,
    bucket: &mut Bucket<U, S, C>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<U>,
    recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
) {
    let (position, net_coin) = bucket::execute_write_writer_flow_for_testing<U, S, C>(
        bucket,
        org,
        config,
        treasury,
        signer_account,
        underlying_in,
        signed_quote,
        clock,
        scenario.ctx(),
    );
    transfer::public_transfer(position, recipient);
    transfer::public_transfer(net_coin, recipient);
}

/// Trader-flow wrapper: forwards the returned `Coin<Call>` to `recipient`.
public fun execute_trader_flow_for_testing<U, S, C>(
    scenario: &mut Scenario,
    bucket: &mut Bucket<U, S, C>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    premium_in: Coin<S>,
    recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
) {
    let call = bucket::execute_write_trader_flow_for_testing<U, S, C>(
        bucket,
        org,
        config,
        treasury,
        signer_account,
        premium_in,
        signed_quote,
        clock,
        scenario.ctx(),
    );
    transfer::public_transfer(call, recipient);
}
