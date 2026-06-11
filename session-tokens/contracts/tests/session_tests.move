#[test_only]
module siws_session::session_tests;

use sui::clock;
use sui::coin;
use sui::sui::SUI;
use sui::test_scenario as ts;
use sui::test_utils;

use siws_session::account::{Self, Account};
use siws_session::app_example;
use siws_session::message;
use siws_session::registry;
use siws_session::session::{Self, SessionCap, SpendLimit};

const HOLDER: address = @0xCAFE;
const HOLDER2: address = @0xCAF2;
const OTHER: address = @0xF00D;
const RECIPIENT: address = @0xBEEF;

fun fill(n: u64, v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < n) { out.push_back(v); i = i + 1; };
    out
}

fun fund(amount: u64, ctx: &mut TxContext): Account {
    let mut acct = account::new_for_testing(fill(32, 0x11), ctx);
    account::deposit(&mut acct, coin::mint_for_testing<SUI>(amount, ctx));
    acct
}

fun allowlist(): vector<vector<u8>> {
    vector[app_example::withdraw_selector()]
}

/// SUI limit: per_tx / total.
fun sui_limit(per_tx: u64, total: u64): vector<SpendLimit> {
    vector[session::new_limit_for_testing(
        account::canonical_type_bytes<SUI>(), per_tx, total,
    )]
}

// --- serializer pins (must match the SDK reference vectors byte-for-byte;
// --- regenerate with `sdk/gen-siwe.mjs`) ---

#[test]
fun test_session_message_reference() {
    let limit_types = vector[
        b"0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        b"0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC",
    ];
    let msg = message::build_session_message(
        @0x1, b"testnet", fill(32, 0x11), @0x2, 7, fill(32, 0x22), 1700000000000,
        &limit_types, &vector[200, 1000000], &vector[500, 5000000],
    );
    let expected = b"siws-session-v2\ndomain: 0x0000000000000000000000000000000000000000000000000000000000000001\nchain: sui:testnet\naccount: 0x1111111111111111111111111111111111111111111111111111111111111111\nsession_key: 0x0000000000000000000000000000000000000000000000000000000000000002\ngeneration: 7\nnonce: 0x2222222222222222222222222222222222222222222222222222222222222222\nexpires_at_ms: 1700000000000\nlimits: 0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI=200/500,0x00000000000000000000000000000000000000000000000000000000000000aa::tusdc::TUSDC=1000000/5000000";
    assert!(msg == expected, 0);
}

#[test]
fun test_session_message_no_limits() {
    let msg = message::build_session_message(
        @0x1, b"testnet", fill(32, 0x11), @0x2, 7, fill(32, 0x22), 1700000000000,
        &vector[], &vector[], &vector[],
    );
    let expected = b"siws-session-v2\ndomain: 0x0000000000000000000000000000000000000000000000000000000000000001\nchain: sui:testnet\naccount: 0x1111111111111111111111111111111111111111111111111111111111111111\nsession_key: 0x0000000000000000000000000000000000000000000000000000000000000002\ngeneration: 7\nnonce: 0x2222222222222222222222222222222222222222222222222222222222222222\nexpires_at_ms: 1700000000000\nlimits: none";
    assert!(msg == expected, 0);
}

#[test]
fun test_revoke_message_reference() {
    let msg = message::build_revoke_message(
        @0x1, b"testnet", fill(32, 0x11), object::id_from_address(@0x3), fill(32, 0x22), 1700000000000,
    );
    let expected = b"siws-session-revoke-v1\ndomain: 0x0000000000000000000000000000000000000000000000000000000000000001\nchain: sui:testnet\naccount: 0x1111111111111111111111111111111111111111111111111111111111111111\naccount_id: 0x0000000000000000000000000000000000000000000000000000000000000003\nnonce: 0x2222222222222222222222222222222222222222222222222222222222222222\nexpires_at_ms: 1700000000000";
    assert!(msg == expected, 0);
}

// --- end-to-end happy path through the example entrypoint ---

#[test]
fun test_app_withdraw_e2e() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let account_id = object::id(&acct);
    let cap = session::mint_for_testing(
        account_id, 0, 9_999_999, sui_limit(200, 500), allowlist(), HOLDER, sc.ctx(),
    );

    app_example::withdraw<SUI>(&cap, &mut acct, &clock, 150, RECIPIENT, sc.ctx());

    assert!(account::balance_of<SUI>(&acct) == 850, 0);
    assert!(account::spent_of<SUI>(&acct, object::id(&cap)) == 150, 1);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

// --- the re-access guarantee: a fresh sign-in (after the previous session
// --- expired) lands on the SAME account with funds intact ---

#[test]
fun test_reaccess_same_account_after_expiry() {
    let mut sc = ts::begin(OTHER);
    let mut clock = clock::create_for_testing(sc.ctx());
    clock.set_for_testing(1_000);
    let mut reg = registry::new_for_testing(b"testnet", sc.ctx());
    let identity = fill(32, 0x11);

    // First sign-in creates + shares the account and mints a cap to HOLDER.
    session::open_for_testing(
        &mut reg, identity, HOLDER, 0, 10_000, sui_limit(200, 500), allowlist(), sc.ctx(),
    );
    ts::next_tx(&mut sc, HOLDER);
    let mut acct = sc.take_shared<Account>();
    let account_id = object::id(&acct);
    account::deposit(&mut acct, coin::mint_for_testing<SUI>(1000, sc.ctx()));

    // The session expires; the user signs in again later with a NEW
    // ephemeral key (e.g. from a different browser).
    clock.set_for_testing(20_000);
    session::open_for_testing(
        &mut reg, identity, HOLDER2, 0, 30_000, sui_limit(200, 500), allowlist(), sc.ctx(),
    );
    ts::next_tx(&mut sc, HOLDER2);
    let cap2 = sc.take_from_address<SessionCap>(HOLDER2);

    // Same account, funds intact, and the new cap is live.
    assert!(session::account_id(&cap2) == account_id, 0);
    assert!(account::balance_of<SUI>(&acct) == 1000, 1);
    app_example::withdraw<SUI>(&cap2, &mut acct, &clock, 150, RECIPIENT, sc.ctx());
    assert!(account::balance_of<SUI>(&acct) == 850, 2);

    ts::return_shared(acct);
    test_utils::destroy(cap2);
    test_utils::destroy(reg);
    clock::destroy_for_testing(clock);
    sc.end();
}

// --- enforcement failure paths (each isolates exactly one violated check) ---

#[test, expected_failure]
fun test_over_per_tx() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    let cap = session::mint_for_testing(
        id, 0, 9_999_999, sui_limit(100, 1000), allowlist(), HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 150, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_over_total() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    let cap = session::mint_for_testing(
        id, 0, 9_999_999, sui_limit(200, 200), allowlist(), HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 150, app_example::withdraw_selector(), HOLDER);
    // cumulative 150 + 100 = 250 > total 200
    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 100, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_spend_type_without_limit() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    // Cap carries a limit for a DIFFERENT type only — SUI is unspendable.
    let limits = vector[session::new_limit_for_testing(b"0xaa::tusdc::TUSDC", 1000, 1000)];
    let cap = session::mint_for_testing(id, 0, 9_999_999, limits, allowlist(), HOLDER, sc.ctx());

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_expired() {
    let mut sc = ts::begin(HOLDER);
    let mut clock = clock::create_for_testing(sc.ctx());
    clock.set_for_testing(1_000_000);
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    let cap = session::mint_for_testing(
        id, 0, 500_000, sui_limit(1000, 1000), allowlist(), HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_revoked() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    // cap generation 5 != account generation 0
    let cap = session::mint_for_testing(
        id, 5, 9_999_999, sui_limit(1000, 1000), allowlist(), HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_wrong_holder() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    let cap = session::mint_for_testing(
        id, 0, 9_999_999, sui_limit(1000, 1000), allowlist(), HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), OTHER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}

#[test, expected_failure]
fun test_not_allowed() {
    let mut sc = ts::begin(HOLDER);
    let clock = clock::create_for_testing(sc.ctx());
    let mut acct = fund(1000, sc.ctx());
    let id = object::id(&acct);
    let cap = session::mint_for_testing(
        id, 0, 9_999_999, sui_limit(1000, 1000), vector[b"other::fn"], HOLDER, sc.ctx(),
    );

    session::authorize_spend<SUI>(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}
