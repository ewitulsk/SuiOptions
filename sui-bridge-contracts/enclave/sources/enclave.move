// Adapted from MystenLabs/nautilus move/enclave/sources/enclave.move
// (Copyright (c) Mysten Labs, SPDX-License-Identifier: Apache-2.0).
//
// On-chain registry of attested Nitro enclaves (bridge_tickets/07 Phase 3). The
// AWS Nitro attestation itself is verified natively by the Sui framework
// (`sui::nitro_attestation`); this module only compares the attested PCRs to an
// approved `EnclaveConfig` and records the enclave's ephemeral pubkey.
//
// Bridge additions over the upstream reference:
//   - `update_enclave_pk`: re-register a fresh ephemeral key into the SAME
//     `Enclave` object (stable object id), so the ticket-08 Seal policy — which
//     binds a node's share to its `Enclave` object id — survives an enclave
//     restart/replacement (spec §6.4). Owner-gated + on-chain-visible (event).
//   - Events on register/update, for the ticket-10 alerting (an anomalous
//     re-registration is an attack signal).
module bridge_enclave::enclave;

use std::bcs;
use std::string::String;
use sui::ed25519;
use sui::event;
use sui::nitro_attestation::NitroAttestationDocument;

use fun to_pcrs as NitroAttestationDocument.to_pcrs;

const EInvalidPCRs: u64 = 0;
const EInvalidConfigVersion: u64 = 1;
const EInvalidCap: u64 = 2;
const EInvalidOwner: u64 = 3;

/// PCR0 = enclave image (EIF), PCR1 = kernel, PCR2 = application.
public struct Pcrs(vector<u8>, vector<u8>, vector<u8>) has copy, drop, store;

/// The approved measurements a registering enclave must match. Governance holds
/// the `Cap` and bumps `version` when the PCRs change (e.g. an approved rebuild).
public struct EnclaveConfig<phantom T> has key {
    id: UID,
    name: String,
    pcrs: Pcrs,
    capability_id: ID,
    version: u64,
}

/// A verified enclave instance and its boot-fresh ephemeral pubkey. One per
/// signer node; its object id is the identity the Seal policy binds to (§6.5).
public struct Enclave<phantom T> has key {
    id: UID,
    pk: vector<u8>,
    config_version: u64,
    owner: address,
}

/// Capability to edit the `EnclaveConfig` (PCRs/name). Held by governance.
public struct Cap<phantom T> has key, store {
    id: UID,
}

public struct IntentMessage<T: drop> has copy, drop {
    intent: u8,
    timestamp_ms: u64,
    payload: T,
}

// --- events (ticket-10 alerting) ---
public struct EnclaveRegistered has copy, drop {
    enclave_id: ID,
    config_version: u64,
    owner: address,
}
public struct EnclavePkUpdated has copy, drop {
    enclave_id: ID,
    owner: address,
    config_version: u64,
}

/// Create a governance `Cap` from a module witness `T`.
public fun new_cap<T: drop>(_: T, ctx: &mut TxContext): Cap<T> {
    Cap { id: object::new(ctx) }
}

public fun create_enclave_config<T: drop>(
    cap: &Cap<T>,
    name: String,
    pcr0: vector<u8>,
    pcr1: vector<u8>,
    pcr2: vector<u8>,
    ctx: &mut TxContext,
) {
    transfer::share_object(EnclaveConfig<T> {
        id: object::new(ctx),
        name,
        pcrs: Pcrs(pcr0, pcr1, pcr2),
        capability_id: cap.id.to_inner(),
        version: 0,
    });
}

/// Permissionlessly register an enclave whose attestation matches the config.
/// The registrant (`ctx.sender()`) becomes the node's operator/owner.
public fun register_enclave<T>(
    enclave_config: &EnclaveConfig<T>,
    document: NitroAttestationDocument,
    ctx: &mut TxContext,
) {
    let pk = enclave_config.load_pk(&document);
    let enclave = Enclave<T> {
        id: object::new(ctx),
        pk,
        config_version: enclave_config.version,
        owner: ctx.sender(),
    };
    event::emit(EnclaveRegistered {
        enclave_id: object::id(&enclave),
        config_version: enclave.config_version,
        owner: enclave.owner,
    });
    transfer::share_object(enclave);
}

/// Re-register a fresh ephemeral key into an EXISTING `Enclave` object after a
/// restart/replacement — keeping the object id stable so the Seal share binding
/// (§6.5/§6.4) still resolves. Owner-gated; re-verifies a fresh attestation.
public fun update_enclave_pk<T>(
    enclave: &mut Enclave<T>,
    enclave_config: &EnclaveConfig<T>,
    document: NitroAttestationDocument,
    ctx: &mut TxContext,
) {
    assert!(enclave.owner == ctx.sender(), EInvalidOwner);
    enclave.pk = enclave_config.load_pk(&document);
    enclave.config_version = enclave_config.version;
    event::emit(EnclavePkUpdated {
        enclave_id: object::id(enclave),
        owner: enclave.owner,
        config_version: enclave.config_version,
    });
}

public fun verify_signature<T, P: drop>(
    enclave: &Enclave<T>,
    intent_scope: u8,
    timestamp_ms: u64,
    payload: P,
    signature: &vector<u8>,
): bool {
    let intent_message = create_intent_message(intent_scope, timestamp_ms, payload);
    let bytes = bcs::to_bytes(&intent_message);
    ed25519::ed25519_verify(signature, &enclave.pk, &bytes)
}

public fun update_pcrs<T: drop>(
    config: &mut EnclaveConfig<T>,
    cap: &Cap<T>,
    pcr0: vector<u8>,
    pcr1: vector<u8>,
    pcr2: vector<u8>,
) {
    cap.assert_is_valid_for_config(config);
    config.pcrs = Pcrs(pcr0, pcr1, pcr2);
    config.version = config.version + 1;
}

public fun update_name<T: drop>(config: &mut EnclaveConfig<T>, cap: &Cap<T>, name: String) {
    cap.assert_is_valid_for_config(config);
    config.name = name;
}

// --- views ---
public fun pcr0<T>(config: &EnclaveConfig<T>): &vector<u8> { &config.pcrs.0 }
public fun pcr1<T>(config: &EnclaveConfig<T>): &vector<u8> { &config.pcrs.1 }
public fun pcr2<T>(config: &EnclaveConfig<T>): &vector<u8> { &config.pcrs.2 }
public fun config_version<T>(config: &EnclaveConfig<T>): u64 { config.version }
public fun pk<T>(enclave: &Enclave<T>): &vector<u8> { &enclave.pk }
public fun owner<T>(enclave: &Enclave<T>): address { enclave.owner }
public fun enclave_config_version<T>(enclave: &Enclave<T>): u64 { enclave.config_version }

/// Retire an enclave whose config version is stale (post-rotation cleanup).
public fun destroy_old_enclave<T>(e: Enclave<T>, config: &EnclaveConfig<T>) {
    assert!(e.config_version < config.version, EInvalidConfigVersion);
    let Enclave { id, .. } = e;
    id.delete();
}

public fun destroy_enclave_by_owner<T>(e: Enclave<T>, ctx: &mut TxContext) {
    assert!(e.owner == ctx.sender(), EInvalidOwner);
    let Enclave { id, .. } = e;
    id.delete();
}

fun assert_is_valid_for_config<T>(cap: &Cap<T>, enclave_config: &EnclaveConfig<T>) {
    assert!(cap.id.to_inner() == enclave_config.capability_id, EInvalidCap);
}

fun load_pk<T>(enclave_config: &EnclaveConfig<T>, document: &NitroAttestationDocument): vector<u8> {
    assert!(document.to_pcrs() == enclave_config.pcrs, EInvalidPCRs);
    (*document.public_key()).destroy_some()
}

fun to_pcrs(document: &NitroAttestationDocument): Pcrs {
    let pcrs = document.pcrs();
    Pcrs(*pcrs[0].value(), *pcrs[1].value(), *pcrs[2].value())
}

public fun create_intent_message<P: drop>(intent: u8, timestamp_ms: u64, payload: P): IntentMessage<P> {
    IntentMessage { intent, timestamp_ms, payload }
}

// --- test-only constructors (the attestation path needs a real doc + hardware,
// so unit tests build the Enclave directly to exercise the registry logic) ---
#[test_only]
public fun deploy_for_testing<T>(pk: vector<u8>, config_version: u64, ctx: &mut TxContext): Enclave<T> {
    Enclave { id: object::new(ctx), pk, config_version, owner: ctx.sender() }
}

#[test_only]
public fun destroy<T>(enclave: Enclave<T>) {
    let Enclave { id, .. } = enclave;
    id.delete();
}
