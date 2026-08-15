#[test_only]
module options_core::quote_tests;

use sui::test_scenario::{Self as ts};

use options_core::admin::{Self, ProtocolConfig};
use options_core::quote;
use options_core::quote_signer;
use options_core::test_helpers as th;

const EXPIRY_MS: u64 = 1_700_000_000_000;

/// These exercise signature / nonce / expiry verification only, which is
/// bucket-independent, so the spec is an arbitrary well-formed one.
fun build_signed_quote(
    protocol_id: vector<u8>,
    signer_id: ID,
    valid_until_ms: u64,
    nonce: u64,
    sig: vector<u8>,
): quote::SignedQuote {
    let q = th::new_test_quote_spec<th::BTC, th::USDC>(
        protocol_id,
        signer_id,
        @0xC3,
        EXPIRY_MS,
        50_000,
        0,
        /* is_put */ false,
        std::u128::max_value!(),
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
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        1_000_000,
        42,
        x"",
    );

    let _q = quote::verify_skip_sig(&mut signer, &config, &sq, &clock);
    assert!(quote_signer::has_nonce(&signer, 42), 0);

    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 1, location = options_core::quote)] // quote_expired
fun test_verify_expired_quote_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let mut clk = th::init_protocol(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    clk.set_for_testing(2_000);
    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        1_000,
        1,
        x"",
    );

    let _q = quote::verify_skip_sig(&mut signer, &config, &sq, &clk);

    ts::return_shared(config);
    ts::return_shared(signer);
    clk.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 2, location = options_core::quote)] // quote_nonce_used
fun test_verify_replay_nonce_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        1_000_000,
        7,
        x"",
    );

    let _q1 = quote::verify_skip_sig(&mut signer, &config, &sq, &clock);
    let _q2 = quote::verify_skip_sig(&mut signer, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 4, location = options_core::quote)] // quote_protocol_mismatch
fun test_verify_protocol_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    let sq = build_signed_quote(
        x"deadbeef",
        object::id(&signer),
        1_000_000,
        1,
        x"",
    );
    let _q = quote::verify_skip_sig(&mut signer, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 6, location = options_core::quote)] // quote_account_mismatch
fun test_verify_signer_mismatch_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    let bogus_signer_id = object::id_from_address(@0xDEAD);
    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        bogus_signer_id,
        1_000_000,
        1,
        x"",
    );
    let _q = quote::verify_skip_sig(&mut signer, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 3, location = options_core::quote)] // quote_signature_invalid — production path
fun test_verify_real_invalid_signature_aborts() {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    th::create_signer(&mut scenario, th::trader_mm_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::admin_addr());
    let config = ts::take_shared<ProtocolConfig>(&scenario);
    let mut signer = th::take_signer(&scenario);

    let sq = build_signed_quote(
        *admin::protocol_id(&config),
        object::id(&signer),
        1_000_000,
        1,
        x"00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    );

    let _q = quote::verify_and_consume_quote(&mut signer, &config, &sq, &clock);

    ts::return_shared(config);
    ts::return_shared(signer);
    clock.destroy_for_testing();
    ts::end(scenario);
}
