#[test_only]
module options_protocol::quote_tests;

use sui::test_scenario::{Self as ts};

use options_protocol::account;
use options_protocol::admin::{Self, ProtocolConfig};
use options_protocol::quote;
use options_protocol::test_helpers as th;

fun build_signed_quote(
    protocol_id: vector<u8>,
    signer_account_id: ID,
    bucket_id: ID,
    valid_until_ms: u64,
    nonce: u64,
    sig: vector<u8>,
): quote::SignedQuote {
    let q = quote::new_quote(
        protocol_id,
        signer_account_id,
        @0xC3,
        bucket_id,
        100,
        50_000,
        valid_until_ms,
        nonce,
    );
    quote::new_signed_quote(q, sig)
}

#[test]
fun test_verify_skip_sig_consumes_nonce() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&acc),
        object::id_from_address(@0xAA),
        1_000_000,
        42,
        x"",
    );

    let _q = quote::verify_skip_sig(&mut acc, &config, &sq, &clock);
    assert!(account::has_nonce(&acc, 42), 0);

    ts::return_shared(config);
    ts::return_shared(acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 1, location = options_protocol::quote)] // quote_expired
fun test_verify_expired_quote_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clk = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    clk.set_for_testing(2_000);
    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&acc),
        object::id_from_address(@0xAA),
        1_000,
        1,
        x"",
    );

    let _q = quote::verify_skip_sig(&mut acc, &config, &sq, &clk);

    ts::return_shared(config);
    ts::return_shared(acc);
    clk.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 2, location = options_protocol::quote)] // quote_nonce_used
fun test_verify_replay_nonce_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&acc),
        object::id_from_address(@0xAA),
        1_000_000,
        7,
        x"",
    );

    let _q1 = quote::verify_skip_sig(&mut acc, &config, &sq, &clock);
    let _q2 = quote::verify_skip_sig(&mut acc, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 4, location = options_protocol::quote)] // quote_protocol_mismatch
fun test_verify_protocol_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    let sq = build_signed_quote(
        x"deadbeef",
        object::id(&acc),
        object::id_from_address(@0xAA),
        1_000_000,
        1,
        x"",
    );
    let _q = quote::verify_skip_sig(&mut acc, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 6, location = options_protocol::quote)] // quote_account_mismatch
fun test_verify_account_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    let bogus_account_id = object::id_from_address(@0xDEAD);
    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        bogus_account_id,
        object::id_from_address(@0xAA),
        1_000_000,
        1,
        x"",
    );
    let _q = quote::verify_skip_sig(&mut acc, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 3, location = options_protocol::quote)] // quote_signature_invalid — production path
fun test_verify_real_invalid_signature_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_account(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut acc = th::take_account(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&acc),
        object::id_from_address(@0xAA),
        1_000_000,
        1,
        x"00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    );

    let _q = quote::verify_and_consume_quote(&mut acc, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(acc);
    clock.destroy_for_testing();
    ts::end(scenario);
}
