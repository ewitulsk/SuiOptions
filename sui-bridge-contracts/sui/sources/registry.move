/// Chain registry + group-key registry + governance/guardian capabilities.
///
/// The registry is the generic seam (bridge-spec.md §7): `internal_id ⇄
/// {native id, family, outbox, inbox, finality}`. A third chain plugs in here
/// and in the per-family verify adapter (see `envelope`), nowhere else.
///
/// Two capabilities, mirroring the spec's separation of duties:
///   - `GuardianCap`    — pause/unpause Outbox + Inbox (circuit breaker, §2.7).
///   - `GovernanceCap`  — edit the registry, register group keys, set threshold.
module sui_bridge::registry;

use sui::table::{Self, Table};
use sui_bridge::chain_id;
use sui_bridge::errors;
use sui_bridge::events;

/// Pause/unpause authority (held by a guardian multisig in production).
public struct GuardianCap has key, store { id: UID }

/// Registry + key-rotation authority (held by governance).
public struct GovernanceCap has key, store { id: UID }

public struct ChainEntry has store, drop {
    /// Authoritative native id (full EVM chainId, Sui chain identifier, …).
    /// The 5-bit family lives in the registry key, recovered via `chain_id`.
    native_identifier: vector<u8>,
    outbox_addr: vector<u8>,
    inbox_addr: vector<u8>,
    /// 0 = EVM confirmation depth, 1 = Sui finalized-checkpoint rule (§4).
    finality_kind: u8,
    finality_value: u64,
}

public struct ChainRegistry has key {
    id: UID,
    chains: Table<u32, ChainEntry>,
}

public struct GroupKey has store, drop {
    scheme_tag: u8,
    pubkey: vector<u8>,
}

/// Registered group public keys, looked up by `group_pubkey_id` from the
/// delivered envelope so keys can rotate without an ABI change.
public struct GroupKeyRegistry has key {
    id: UID,
    keys: Table<u32, GroupKey>,
    /// Signer threshold (k-of-n) — governance metadata. Not verified on-chain:
    /// a single aggregated threshold signature simply verifies against the
    /// group pubkey. Surfaced for transparency/rotation tooling.
    threshold_k: u16,
    threshold_n: u16,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(ChainRegistry {
        id: object::new(ctx),
        chains: table::new(ctx),
    });
    transfer::share_object(GroupKeyRegistry {
        id: object::new(ctx),
        keys: table::new(ctx),
        threshold_k: 1,
        threshold_n: 1,
    });
    transfer::public_transfer(GuardianCap { id: object::new(ctx) }, ctx.sender());
    transfer::public_transfer(GovernanceCap { id: object::new(ctx) }, ctx.sender());
}

// --- chain registry (governance) ---

/// Register a chain. The family is derived from `internal_id`'s top bits (see
/// `chain_id`) and validated; it is not passed or stored separately.
public fun register_chain(
    _: &GovernanceCap,
    registry: &mut ChainRegistry,
    internal_id: u32,
    native_identifier: vector<u8>,
    outbox_addr: vector<u8>,
    inbox_addr: vector<u8>,
    finality_kind: u8,
    finality_value: u64,
) {
    assert!(!registry.chains.contains(internal_id), errors::chain_already_registered());
    let fam = chain_id::family(internal_id);
    assert!(chain_id::is_valid_family(fam), errors::unknown_family());
    registry.chains.add(internal_id, ChainEntry {
        native_identifier,
        outbox_addr,
        inbox_addr,
        finality_kind,
        finality_value,
    });
    events::emit_chain_registered(internal_id, fam);
}

public fun is_registered(registry: &ChainRegistry, internal_id: u32): bool {
    registry.chains.contains(internal_id)
}

public fun family(registry: &ChainRegistry, internal_id: u32): u8 {
    assert!(registry.chains.contains(internal_id), errors::chain_not_registered());
    chain_id::family(internal_id)
}

public fun finality(registry: &ChainRegistry, internal_id: u32): (u8, u64) {
    assert!(registry.chains.contains(internal_id), errors::chain_not_registered());
    let e = registry.chains.borrow(internal_id);
    (e.finality_kind, e.finality_value)
}

// --- group-key registry (governance / rotation) ---

public fun register_group_key(
    _: &GovernanceCap,
    keys: &mut GroupKeyRegistry,
    group_pubkey_id: u32,
    scheme_tag: u8,
    pubkey: vector<u8>,
) {
    assert!(!keys.keys.contains(group_pubkey_id), errors::group_key_already_registered());
    keys.keys.add(group_pubkey_id, GroupKey { scheme_tag, pubkey });
    events::emit_group_key_registered(group_pubkey_id, scheme_tag);
}

public fun set_signer_threshold(_: &GovernanceCap, keys: &mut GroupKeyRegistry, k: u16, n: u16) {
    assert!(k >= 1 && k <= n, errors::invalid_threshold());
    keys.threshold_k = k;
    keys.threshold_n = n;
}

public fun has_group_key(keys: &GroupKeyRegistry, group_pubkey_id: u32): bool {
    keys.keys.contains(group_pubkey_id)
}

/// Returns `(scheme_tag, pubkey)` for a registered group key, aborting if the
/// id is unknown. Used by the Inbox verify path.
public fun group_key(keys: &GroupKeyRegistry, group_pubkey_id: u32): (u8, vector<u8>) {
    assert!(keys.keys.contains(group_pubkey_id), errors::group_key_not_registered());
    let gk = keys.keys.borrow(group_pubkey_id);
    (gk.scheme_tag, gk.pubkey)
}

public fun threshold(keys: &GroupKeyRegistry): (u16, u16) {
    (keys.threshold_k, keys.threshold_n)
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx);
}
