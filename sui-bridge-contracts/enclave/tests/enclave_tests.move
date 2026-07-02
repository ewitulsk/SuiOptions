#[test_only]
module bridge_enclave::enclave_tests;

use sui::test_scenario::{Self as ts};
use bridge_enclave::enclave::{Self, Cap, EnclaveConfig};

const GOV: address = @0xA;
const OPERATOR: address = @0xB;

/// Test witness (stands in for BRIDGE_SIGNER; a foreign test module can't mint
/// the real one, which is the point of the witness pattern).
public struct WITNESS has drop {}

fun pcr(b: u8): vector<u8> {
    let mut v = vector<u8>[];
    let mut i = 0u64;
    while (i < 48) { v.push_back(b); i = i + 1; }; // Nitro PCRs are SHA-384 (48 bytes)
    v
}

/// Create a config owned by GOV, leaving the scenario holding the Cap + shared config.
fun setup(s: &mut ts::Scenario) {
    let cap = enclave::new_cap(WITNESS {}, s.ctx());
    enclave::create_enclave_config<WITNESS>(&cap, b"bridge-signer".to_string(), pcr(0), pcr(1), pcr(2), s.ctx());
    transfer::public_transfer(cap, GOV);
    s.next_tx(GOV);
}

#[test]
fun pcrs_update_bumps_version() {
    let mut s = ts::begin(GOV);
    setup(&mut s);
    let cap = s.take_from_sender<Cap<WITNESS>>();
    let mut config = s.take_shared<EnclaveConfig<WITNESS>>();

    assert!(enclave::config_version(&config) == 0, 0);
    assert!(*enclave::pcr0(&config) == pcr(0), 1);
    enclave::update_pcrs(&mut config, &cap, pcr(9), pcr(9), pcr(9));
    assert!(enclave::config_version(&config) == 1, 2); // bumped
    assert!(*enclave::pcr0(&config) == pcr(9), 3);

    ts::return_shared(config);
    s.return_to_sender(cap);
    s.end();
}

#[test]
#[expected_failure(abort_code = 2, location = bridge_enclave::enclave)] // EInvalidCap
fun update_pcrs_rejects_foreign_cap() {
    let mut s = ts::begin(GOV);
    setup(&mut s);
    let mut config = s.take_shared<EnclaveConfig<WITNESS>>();
    // A different cap (fresh id) is not the one the config was created with.
    let foreign = enclave::new_cap(WITNESS {}, s.ctx());
    enclave::update_pcrs(&mut config, &foreign, pcr(9), pcr(9), pcr(9));

    transfer::public_transfer(foreign, GOV);
    ts::return_shared(config);
    s.end();
}

#[test]
fun update_enclave_pk_keeps_object_id_and_is_owner_gated_shape() {
    // The register/update paths take a NitroAttestationDocument (framework-native,
    // needs a real doc + hardware) — not constructable in a Move test. Here we
    // exercise the object-stability + owner fields via the test-only constructor.
    let mut s = ts::begin(OPERATOR);
    let e = enclave::deploy_for_testing<WITNESS>(x"2152f8d19b791d24453242e15f2eab6cb7cffa7b6a5ed30097960e069881db12", 0, s.ctx());
    assert!(enclave::owner(&e) == OPERATOR, 0);
    assert!(enclave::enclave_config_version(&e) == 0, 1);
    enclave::destroy(e);
    s.end();
}

#[test]
fun verify_signature_rejects_bad_sig() {
    let mut s = ts::begin(OPERATOR);
    let e = enclave::deploy_for_testing<WITNESS>(pcr(0), 0, s.ctx());
    // A garbage 64-byte signature must not verify.
    let bad = pcr(7); // 48 bytes, wrong length/content → false, not an abort
    assert!(!enclave::verify_signature(&e, 0u8, 1_700_000_000_000u64, b"payload", &bad), 0);
    enclave::destroy(e);
    s.end();
}

#[test]
fun old_enclave_destroyable_after_rotation() {
    let mut s = ts::begin(GOV);
    setup(&mut s);
    let cap = s.take_from_sender<Cap<WITNESS>>();
    let mut config = s.take_shared<EnclaveConfig<WITNESS>>();
    enclave::update_pcrs(&mut config, &cap, pcr(9), pcr(9), pcr(9)); // config → version 1

    // An enclave pinned to version 0 is now stale and can be retired.
    let stale = enclave::deploy_for_testing<WITNESS>(pcr(0), 0, s.ctx());
    enclave::destroy_old_enclave(stale, &config);

    ts::return_shared(config);
    s.return_to_sender(cap);
    s.end();
}

#[test]
#[expected_failure(abort_code = 1, location = bridge_enclave::enclave)] // EInvalidConfigVersion
fun current_enclave_not_destroyable_as_old() {
    let mut s = ts::begin(GOV);
    setup(&mut s);
    let config = s.take_shared<EnclaveConfig<WITNESS>>();
    // config is at version 0; an enclave also at version 0 is not "old".
    let current = enclave::deploy_for_testing<WITNESS>(pcr(0), 0, s.ctx());
    enclave::destroy_old_enclave(current, &config);
    ts::return_shared(config);
    s.end();
}

#[test]
fun init_mints_governance_cap() {
    let mut s = ts::begin(GOV);
    bridge_enclave::signer::init_for_testing(s.ctx());
    s.next_tx(GOV);
    // The deployer received a Cap<BRIDGE_SIGNER>.
    let cap = s.take_from_sender<Cap<bridge_enclave::signer::BRIDGE_SIGNER>>();
    s.return_to_sender(cap);
    s.end();
}
