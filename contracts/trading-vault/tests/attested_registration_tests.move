/// Curator self-serve external-account registration, authorized by an
/// ed25519 attestation from the protocol registrar instead of an
/// AdminCap (`vault::set_external_account_attested`).
///
/// The happy-path vector below is signed OFFLINE (Move cannot sign), so
/// it is pinned to the vault id `test_scenario` deterministically mints
/// for this module's setup. If `VAULT_ID` ever stops matching, the setup
/// changed — re-sign `EXTERNAL_REG_DOMAIN ‖ new_vault_id ‖ account` with
/// the fixed seed 0x00..0x1f and update `SIG`.
#[test_only]
module trading_vault::attested_registration_tests;

use std::type_name;
use sui::test_scenario::{Self as ts, Scenario};

use trading_vault::test_helpers::{Self as th, TestOracle, RogueOracle};
use trading_vault::registry::{Self, OracleRegistry};
use trading_vault::vault::{Self, CuratorCap, TradingVault};

/// ed25519 pubkey of the fixed test seed 0x00..0x1f.
const PUBKEY: vector<u8> = x"03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8";
/// Vault id `new_default_vault` mints under this module's setup.
const VAULT_ID: address = @0x1611edd9a9d42dbcd9ae773ffa22be0f6017b00590959dd5c767e4efcd34cd0b;
/// Signature over the registration message for (VAULT_ID, EXTERNAL_ADDR).
const SIG: vector<u8> =
    x"83ef19882fe0d7667430db10775a0283c398ee7e88d2f35ec333983e783c422b7a69630b6471ad4930ce082f0954542635cb1d73517ff443c2b01b73ca75fb0f";
const EXTERNAL_ADDR: address = @0xF00D;

const BUDGET_BPS: u64 = 2_000;
const DAILY_BPS: u64 = 1_000;

/// Fresh scenario + protocol + default USDC vault, registrar pubkey
/// seeded unless `pubkey` is empty.
fun setup(pubkey: vector<u8>): Scenario {
    let mut scenario = ts::begin(th::admin_addr());
    let clock = th::init_protocol(&mut scenario);
    clock.destroy_for_testing();
    let vault_id = th::new_default_vault(&mut scenario);
    assert!(vault_id.to_address() == VAULT_ID, 0);

    if (!pubkey.is_empty()) {
        ts::next_tx(&mut scenario, th::admin_addr());
        let cap = th::take_admin_cap(&scenario);
        let mut cfg = th::take_protocol_config(&scenario);
        registry::set_registrar_pubkey(&cap, &mut cfg, pubkey);
        ts::return_shared(cfg);
        th::return_admin_cap(&scenario, cap);
    };
    scenario
}

/// Curator-signed attested registration with TestOracle pinned.
fun register(scenario: &mut Scenario, budget_bps: u64, daily_bps: u64, sig: vector<u8>) {
    register_with_oracle<TestOracle>(scenario, budget_bps, daily_bps, sig)
}

fun register_with_oracle<O>(
    scenario: &mut Scenario,
    budget_bps: u64,
    daily_bps: u64,
    sig: vector<u8>,
) {
    ts::next_tx(scenario, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(scenario);
    let cfg = th::take_protocol_config(scenario);
    let oreg = ts::take_shared<OracleRegistry>(scenario);
    let cap = ts::take_from_sender<CuratorCap>(scenario);
    vault::set_external_account_attested(
        &cap,
        &mut v,
        &cfg,
        &oreg,
        EXTERNAL_ADDR,
        type_name::with_defining_ids<O>(),
        budget_bps,
        daily_bps,
        sig,
    );
    ts::return_to_sender(scenario, cap);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
}

// ═════════════════════════ message construction ═════════════════════════

/// The exact bytes the hedge-signer must sign: 18-byte domain tag, then
/// the two addresses as raw 32-byte big-endian words.
#[test]
fun registration_message_matches_signer_layout() {
    let msg = vault::external_registration_message(@0x1, @0x2);
    assert!(msg.length() == 82, 0);
    assert!(
        msg == x"74765f65787465726e616c5f7265675f763100000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000002",
        1,
    );
}

// ═══════════════════════════════ happy path ══════════════════════════════

#[test]
fun attested_registration_registers_the_account() {
    let mut scenario = setup(PUBKEY);
    register(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);

    ts::next_tx(&mut scenario, th::curator_addr());
    let v = ts::take_shared<TradingVault>(&scenario);
    assert!(vault::has_external_account(&v), 0);
    assert!(vault::external_account(&v) == EXTERNAL_ADDR, 1);
    assert!(vault::external_equity_oracle(&v) == type_name::with_defining_ids<TestOracle>(), 2);
    assert!(vault::external_exposure(&v) == 0, 3);
    let (budget, daily, released, _window) = vault::external_limits(&v);
    assert!(budget == BUDGET_BPS, 4);
    assert!(daily == DAILY_BPS, 5);
    assert!(released == 0, 6);
    ts::return_shared(v);
    ts::end(scenario);
}

// ═════════════════════════════ first-set-only ════════════════════════════

#[test]
#[expected_failure(abort_code = 105, location = trading_vault::vault)] // external_already_set
fun attestation_cannot_be_replayed_onto_a_registered_vault() {
    let mut scenario = setup(PUBKEY);
    register(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);
    register(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 105, location = trading_vault::vault)] // external_already_set
fun attestation_cannot_repoint_an_admin_registered_account() {
    let mut scenario = setup(PUBKEY);

    ts::next_tx(&mut scenario, th::admin_addr());
    let admin_cap = th::take_admin_cap(&scenario);
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    vault::set_external_account(
        &admin_cap,
        &mut v,
        &oreg,
        @0xBEEF,
        type_name::with_defining_ids<TestOracle>(),
        5_000,
        2_500,
    );
    ts::return_shared(oreg);
    ts::return_shared(v);
    th::return_admin_cap(&scenario, admin_cap);

    register(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);
    ts::end(scenario);
}

// ═══════════════════════════════ the gates ═══════════════════════════════

#[test]
#[expected_failure(abort_code = 106, location = trading_vault::vault)] // attested_limits_exceeded
fun attested_budget_is_capped() {
    let mut scenario = setup(PUBKEY);
    register(&mut scenario, BUDGET_BPS + 1, DAILY_BPS, SIG);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 106, location = trading_vault::vault)] // attested_limits_exceeded
fun attested_daily_release_is_capped() {
    let mut scenario = setup(PUBKEY);
    register(&mut scenario, BUDGET_BPS, DAILY_BPS + 1, SIG);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 76, location = trading_vault::vault)] // oracle_not_allowed
fun attested_equity_oracle_must_be_allowlisted() {
    let mut scenario = setup(PUBKEY);
    register_with_oracle<RogueOracle>(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 107, location = trading_vault::vault)] // attestation_disabled
fun attested_path_fails_closed_until_the_pubkey_is_seeded() {
    let mut scenario = setup(vector[]);
    register(&mut scenario, BUDGET_BPS, DAILY_BPS, SIG);
    ts::end(scenario);
}

#[test]
#[expected_failure(abort_code = 108, location = trading_vault::vault)] // bad_attestation
fun a_signature_from_another_key_is_rejected() {
    let mut scenario = setup(PUBKEY);
    let mut forged = SIG;
    *forged.borrow_mut(0) = 0x00;
    register(&mut scenario, BUDGET_BPS, DAILY_BPS, forged);
    ts::end(scenario);
}

/// A signature valid for a DIFFERENT account does not authorize this one.
#[test]
#[expected_failure(abort_code = 108, location = trading_vault::vault)] // bad_attestation
fun a_signature_over_another_account_is_rejected() {
    let mut scenario = setup(PUBKEY);
    ts::next_tx(&mut scenario, th::curator_addr());
    let mut v = ts::take_shared<TradingVault>(&scenario);
    let cfg = th::take_protocol_config(&scenario);
    let oreg = ts::take_shared<OracleRegistry>(&scenario);
    let cap = ts::take_from_sender<CuratorCap>(&scenario);
    vault::set_external_account_attested(
        &cap,
        &mut v,
        &cfg,
        &oreg,
        @0xBEEF, // signed for EXTERNAL_ADDR, not this one
        type_name::with_defining_ids<TestOracle>(),
        BUDGET_BPS,
        DAILY_BPS,
        SIG,
    );
    ts::return_to_sender(&scenario, cap);
    ts::return_shared(oreg);
    ts::return_shared(cfg);
    ts::return_shared(v);
    ts::end(scenario);
}

// ══════════════════════════════ admin setter ═════════════════════════════

#[test]
#[expected_failure(abort_code = 90, location = trading_vault::registry)] // config_invalid
fun registrar_pubkey_must_be_32_bytes_or_empty() {
    let mut scenario = setup(vector[]);
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut cfg = th::take_protocol_config(&scenario);
    registry::set_registrar_pubkey(&cap, &mut cfg, x"0011");
    ts::return_shared(cfg);
    th::return_admin_cap(&scenario, cap);
    ts::end(scenario);
}

#[test]
fun registrar_pubkey_round_trips_and_clears() {
    let mut scenario = setup(PUBKEY);
    ts::next_tx(&mut scenario, th::admin_addr());
    let cap = th::take_admin_cap(&scenario);
    let mut cfg = th::take_protocol_config(&scenario);
    let expected = PUBKEY;
    assert!(registry::registrar_pubkey(&cfg) == expected, 0);
    registry::set_registrar_pubkey(&cap, &mut cfg, vector[]);
    assert!(registry::registrar_pubkey(&cfg).is_empty(), 1);
    ts::return_shared(cfg);
    th::return_admin_cap(&scenario, cap);
    ts::end(scenario);
}
