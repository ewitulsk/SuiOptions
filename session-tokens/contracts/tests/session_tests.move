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
use siws_session::session;

const HOLDER: address = @0xCAFE;
const OTHER: address = @0xF00D;
const RECIPIENT: address = @0xBEEF;

fun fill(n: u64, v: u8): vector<u8> {
    let mut out = vector::empty<u8>();
    let mut i = 0;
    while (i < n) { out.push_back(v); i = i + 1; };
    out
}

fun fund(amount: u64, ctx: &mut TxContext): Account<SUI> {
    let mut acct = account::new_for_testing<SUI>(fill(32, 0x11), ctx);
    account::deposit(&mut acct, coin::mint_for_testing<SUI>(amount, ctx));
    acct
}

fun allowlist(): vector<vector<u8>> {
    vector[app_example::withdraw_selector()]
}

// --- serializer pins (must match the SDK reference vectors byte-for-byte) ---

#[test]
fun test_session_message_reference() {
    let msg = message::build_session_message(
        @0x1, b"testnet", fill(32, 0x11), @0x2, 7, fill(32, 0x22), 1700000000000,
    );
    let expected = b"siws-session-v1\ndomain: 0x0000000000000000000000000000000000000000000000000000000000000001\nchain: sui:testnet\naccount: 0x1111111111111111111111111111111111111111111111111111111111111111\nsession_key: 0x0000000000000000000000000000000000000000000000000000000000000002\ngeneration: 7\nnonce: 0x2222222222222222222222222222222222222222222222222222222222222222\nexpires_at_ms: 1700000000000";
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
        account_id, 0, 9_999_999, 500, 200, allowlist(), HOLDER, sc.ctx(),
    );

    app_example::withdraw(&cap, &mut acct, &clock, 150, RECIPIENT, sc.ctx());

    assert!(account::balance_value(&acct) == 850, 0);
    assert!(account::spent_of(&acct, object::id(&cap)) == 150, 1);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
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
    let cap = session::mint_for_testing(id, 0, 9_999_999, 1000, 100, allowlist(), HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 150, app_example::withdraw_selector(), HOLDER);

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
    let cap = session::mint_for_testing(id, 0, 9_999_999, 200, 200, allowlist(), HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 150, app_example::withdraw_selector(), HOLDER);
    // cumulative 150 + 100 = 250 > spend_cap 200
    session::authorize_for_testing(&cap, &mut acct, &clock, 100, app_example::withdraw_selector(), HOLDER);

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
    let cap = session::mint_for_testing(id, 0, 500_000, 1000, 1000, allowlist(), HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

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
    let cap = session::mint_for_testing(id, 5, 9_999_999, 1000, 1000, allowlist(), HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

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
    let cap = session::mint_for_testing(id, 0, 9_999_999, 1000, 1000, allowlist(), HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), OTHER);

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
    let cap = session::mint_for_testing(id, 0, 9_999_999, 1000, 1000, vector[b"other::fn"], HOLDER, sc.ctx());

    session::authorize_for_testing(&cap, &mut acct, &clock, 10, app_example::withdraw_selector(), HOLDER);

    test_utils::destroy(acct);
    test_utils::destroy(cap);
    clock::destroy_for_testing(clock);
    sc.end();
}
