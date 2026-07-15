#[test_only]
module options_core::quote_signer_tests;

use sui::clock;
use sui::test_scenario::{Self as ts};

use options_core::quote_signer;
use options_core::test_helpers as th;

#[test]
fun test_create_and_share_signer() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let signer = th::take_signer(&scenario);
    assert!(quote_signer::owner(&signer) == th::writer_addr(), 0);
    assert!(*quote_signer::signing_pubkey(&signer) == th::pubkey_a(), 0);
    ts::return_shared(signer);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 15, location = options_core::quote_signer)] // not_owner
fun test_set_signing_key_not_owner_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::stranger_addr());
    let mut signer = th::take_signer(&scenario);
    quote_signer::set_quote_signing_key(
        &mut signer, th::scheme_ed25519(), th::pubkey_b(), scenario.ctx(),
    );
    ts::return_shared(signer);
    ts::end(scenario);
}

#[test]
fun test_set_signing_key_owner_succeeds() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer(&mut scenario, th::writer_addr(), th::pubkey_a());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut signer = th::take_signer(&scenario);
    quote_signer::set_quote_signing_key(
        &mut signer, th::scheme_ed25519(), th::pubkey_b(), scenario.ctx(),
    );
    assert!(*quote_signer::signing_pubkey(&signer) == th::pubkey_b(), 0);
    assert!(quote_signer::signing_scheme(&signer) == th::scheme_ed25519(), 0);
    ts::return_shared(signer);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 23, location = options_core::quote_signer)] // invalid_signing_scheme
fun test_create_signer_rejects_unknown_scheme() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer_with_scheme(&mut scenario, th::writer_addr(), 9, th::pubkey_a());
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 24, location = options_core::quote_signer)] // invalid_pubkey_length
fun test_create_signer_rejects_wrong_length() {
    let mut scenario = ts::begin(th::writer_addr());
    // Ed25519 scheme but 33-byte pubkey — should abort.
    let bad = x"d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511aff";
    th::create_signer_with_scheme(&mut scenario, th::writer_addr(), th::scheme_ed25519(), bad);
    ts::end(scenario);
}

#[test]
fun test_prune_nonce_after_expiry() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer(&mut scenario, th::writer_addr(), th::pubkey_a());
    let mut clk = clock::create_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut signer = th::take_signer(&scenario);
    quote_signer::consume_nonce(&mut signer, 7, 1000);
    assert!(quote_signer::has_nonce(&signer, 7), 0);

    clk.set_for_testing(1001);
    quote_signer::prune_nonce(&mut signer, 7, &clk);
    assert!(!quote_signer::has_nonce(&signer, 7), 0);

    ts::return_shared(signer);
    clk.destroy_for_testing();
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 19, location = options_core::quote_signer)] // nonce_still_valid
fun test_prune_nonce_before_expiry_aborts() {
    let mut scenario = ts::begin(th::writer_addr());
    th::create_signer(&mut scenario, th::writer_addr(), th::pubkey_a());
    let mut clk = clock::create_for_testing(scenario.ctx());

    ts::next_tx(&mut scenario, th::writer_addr());
    let mut signer = th::take_signer(&scenario);
    quote_signer::consume_nonce(&mut signer, 9, 5000);

    clk.set_for_testing(1000);
    quote_signer::prune_nonce(&mut signer, 9, &clk);

    ts::return_shared(signer);
    clk.destroy_for_testing();
    ts::end(scenario);
}
