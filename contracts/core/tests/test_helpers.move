#[test_only]
module options_core::test_helpers;

use std::string;
use sui::clock::{Self, Clock};
use sui::coin;
use sui::test_scenario::{Self as ts, Scenario};

use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket;
use options_core::option_coin;
use whitelist::whitelist::{Self, AdminCap as WlAdminCap, Whitelist};
use options_core::put_bucket;
use options_core::quote::{Self, Quote};
use options_core::quote_signer::{Self, QuoteSigner};
use options_core::treasury::{Self, Treasury};

public struct USDC has drop {}
public struct BTC has drop {}

/// Per-bucket option-coin marker types. Each bucket needs a distinct `Call`
/// type for true isolation; tests that create more than one bucket use a
/// different marker per bucket. `create_treasury_cap_for_testing` lets us
/// forge a fresh, zero-supply cap for an arbitrary type without an OTW.
public struct CALL has drop {}
public struct CALL2 has drop {}
public struct CALL3 has drop {}

/// Per-bucket put-coin marker types — the put analog of CALL/CALL2/CALL3.
public struct PUT has drop {}
public struct PUT2 has drop {}
public struct PUT3 has drop {}

/// Create and share a bucket for `(U, S, C)` with a fresh test option-coin
/// treasury cap. Runs as the admin. Mirrors what the scheduler does on chain
/// (publish a coin package, then `create_bucket` with the new cap), minus the
/// publish step.
public fun new_bucket<U, S, C>(
    scenario: &mut Scenario,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    ts::next_tx(scenario, admin_addr());
    let tcap = coin::create_treasury_cap_for_testing<C>(scenario.ctx());
    bucket::create_bucket_for_testing<U, S, C>(
        tcap, expiry_ms, strike, strike_scale, scenario.ctx(),
    );
}

/// Create and share a cash-secured-put bucket for `(U, S, P)` with a fresh
/// test put-coin treasury cap. The put analog of `new_bucket`.
public fun new_put_bucket<U, S, P>(
    scenario: &mut Scenario,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) {
    ts::next_tx(scenario, admin_addr());
    let tcap = coin::create_treasury_cap_for_testing<P>(scenario.ctx());
    put_bucket::create_put_bucket_for_testing<U, S, P>(
        tcap, expiry_ms, strike, strike_scale, scenario.ctx(),
    );
}

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

/// Initialize protocol: AdminCap to admin_addr, ProtocolConfig shared, Treasury
/// shared, `Whitelist` shared (with its own whitelist AdminCap to admin_addr).
/// Whitelists every named test actor EXCEPT `stranger_addr` (the negative
/// fixture for ingress-gate tests). Returns a fresh test Clock.
public fun init_protocol(scenario: &mut Scenario): Clock {
    ts::next_tx(scenario, admin_addr());
    admin::init_for_testing(scenario.ctx());
    whitelist::init_for_testing(scenario.ctx());

    ts::next_tx(scenario, admin_addr());
    let admin_cap = ts::take_from_sender<AdminCap>(scenario);
    treasury::create_and_share(&admin_cap, scenario.ctx());
    ts::return_to_sender(scenario, admin_cap);

    ts::next_tx(scenario, admin_addr());
    let wl_cap = ts::take_from_sender<WlAdminCap>(scenario);
    let mut wl = take_whitelist(scenario);
    whitelist::add_member(&wl_cap, &mut wl, admin_addr());
    whitelist::add_member(&wl_cap, &mut wl, writer_addr());
    whitelist::add_member(&wl_cap, &mut wl, trader_mm_addr());
    whitelist::add_member(&wl_cap, &mut wl, trader_addr());
    whitelist::add_member(&wl_cap, &mut wl, writer_mm_addr());
    ts::return_shared(wl);
    ts::return_to_sender(scenario, wl_cap);

    ts::next_tx(scenario, admin_addr());
    clock::create_for_testing(scenario.ctx())
}

public fun scheme_ed25519(): u8 { 0 }
public fun scheme_secp256k1(): u8 { 1 }
public fun scheme_secp256r1(): u8 { 2 }

/// Create and share a QuoteSigner owned by `owner` with an Ed25519 signing
/// key. Default for existing tests — see `create_signer_with_scheme` for the
/// per-scheme variant.
public fun create_signer(scenario: &mut Scenario, owner: address, pubkey: vector<u8>) {
    create_signer_with_scheme(scenario, owner, scheme_ed25519(), pubkey)
}

/// Create and share a QuoteSigner using an explicit signing scheme byte.
public fun create_signer_with_scheme(
    scenario: &mut Scenario,
    owner: address,
    scheme: u8,
    pubkey: vector<u8>,
) {
    ts::next_tx(scenario, owner);
    quote_signer::create_and_share_signer(scheme, pubkey, scenario.ctx());
}

/// Build a Quote against an explicit spec, filling the routing fields with
/// test dummies: `collateral_source` = the signer's own id (released funds
/// are minted inline in core tests, so the source is never dereferenced) and
/// a placeholder release package/module. `max_total_written` is unbounded —
/// use `new_test_quote_bounded` to exercise the queue gate.
public fun new_test_quote_spec<U, S>(
    protocol_id: vector<u8>,
    signer_id: ID,
    signer_token_recipient: address,
    expiry_ms: u64,
    strike_sig: u64,
    strike_exp: u8,
    is_put: bool,
    max_total_written: u128,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    quote::new_quote<U, S>(
        protocol_id,
        signer_id,
        signer_id, // collateral_source dummy
        @0x0,
        string::utf8(b"mm_collateral"),
        signer_token_recipient,
        expiry_ms,
        strike_sig,
        strike_exp,
        is_put,
        max_total_written,
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    )
}

/// A quote whose spec matches `bucket` exactly, with no queue bound.
public fun new_test_quote<U, S, C>(
    protocol_id: vector<u8>,
    signer_id: ID,
    signer_token_recipient: address,
    bucket: &bucket::Bucket<U, S, C>,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    new_test_quote_bounded<U, S, C>(
        protocol_id,
        signer_id,
        signer_token_recipient,
        bucket,
        std::u128::max_value!(),
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    )
}

/// As `new_test_quote`, with an explicit `max_total_written` queue bound.
public fun new_test_quote_bounded<U, S, C>(
    protocol_id: vector<u8>,
    signer_id: ID,
    signer_token_recipient: address,
    bucket: &bucket::Bucket<U, S, C>,
    max_total_written: u128,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    let (sig, exp) = option_coin::normalize_strike_for_testing(
        bucket::strike(bucket),
        bucket::strike_scale(bucket),
    );
    new_test_quote_spec<U, S>(
        protocol_id,
        signer_id,
        signer_token_recipient,
        bucket::expiry_ms(bucket),
        sig,
        exp,
        /* is_put */ false,
        max_total_written,
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    )
}

/// Put twin of `new_test_quote`.
public fun new_test_quote_put<U, S, P>(
    protocol_id: vector<u8>,
    signer_id: ID,
    signer_token_recipient: address,
    bucket: &put_bucket::PutBucket<U, S, P>,
    write_amount: u64,
    premium: u64,
    valid_until_ms: u64,
    nonce: u64,
): Quote {
    let (sig, exp) = option_coin::normalize_strike_for_testing(
        put_bucket::strike(bucket),
        put_bucket::strike_scale(bucket),
    );
    new_test_quote_spec<U, S>(
        protocol_id,
        signer_id,
        signer_token_recipient,
        put_bucket::expiry_ms(bucket),
        sig,
        exp,
        /* is_put */ true,
        std::u128::max_value!(),
        write_amount,
        premium,
        valid_until_ms,
        nonce,
    )
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

public fun take_treasury(scenario: &Scenario): Treasury {
    ts::take_shared<Treasury>(scenario)
}

public fun take_signer(scenario: &Scenario): QuoteSigner {
    ts::take_shared<QuoteSigner>(scenario)
}
