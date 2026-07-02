/// Outbox (one per chain): apps call `send` to emit a cross-chain message. It
/// assigns a per-destination nonce, computes the canonical keccak256 digest,
/// and emits `MessageCommitted` so the signer group can observe the exact
/// preimage deterministically (bridge-spec.md §2.4).
///
/// The calling app proves its identity by passing a reference to a `UID` it
/// owns; `src_app` is that object's 32-byte ID. Only the module that defines
/// the object can produce a `&UID` for it, so an app's address cannot be
/// spoofed by a third party.
module sui_bridge::outbox;

use sui::table::{Self, Table};
use sui_bridge::errors;
use sui_bridge::events;
use sui_bridge::message;
use sui_bridge::registry::{GovernanceCap, GuardianCap};

public struct Outbox has key {
    id: UID,
    /// Internal registry id of THIS chain (the source for everything it emits).
    src_chain_id: u32,
    /// Digest domain separator (spec §2.2), derived from the deployment salt.
    domain_sep: vector<u8>,
    /// `next_nonce[dst_chain_id]` — monotonic per (src, dst) lane (§2.6).
    next_nonce: Table<u32, u64>,
    paused: bool,
}

/// Create + share an Outbox for this chain. Governance-gated. `deployment_salt`
/// is the 32-byte per-deployment salt; the digest separator is derived + stored.
public fun create(
    _: &GovernanceCap,
    src_chain_id: u32,
    deployment_salt: vector<u8>,
    ctx: &mut TxContext,
): ID {
    let outbox = Outbox {
        id: object::new(ctx),
        src_chain_id,
        domain_sep: message::derive_domain_sep(deployment_salt),
        next_nonce: table::new(ctx),
        paused: false,
    };
    let id = object::id(&outbox);
    transfer::share_object(outbox);
    id
}

/// Emit a message addressed to `dst_app` on `dst_chain_id`.
/// `app` is the caller's own object, proving `src_app`. Returns the assigned
/// nonce and the canonical message hash.
public fun send(
    outbox: &mut Outbox,
    app: &UID,
    dst_chain_id: u32,
    dst_app: vector<u8>,
    payload: vector<u8>,
): (u64, vector<u8>) {
    assert!(!outbox.paused, errors::outbox_paused());

    let nonce = next_nonce(outbox, dst_chain_id);
    let src_app = object::uid_to_bytes(app);
    let m = message::new(
        outbox.src_chain_id,
        dst_chain_id,
        nonce,
        src_app,
        dst_app,
        payload,
    );
    let message_hash = message::hash(&m, outbox.domain_sep);

    *outbox.next_nonce.borrow_mut(dst_chain_id) = nonce + 1;

    events::emit_message_committed(
        object::id(outbox),
        message_hash,
        outbox.src_chain_id,
        dst_chain_id,
        nonce,
        src_app,
        dst_app,
        payload,
    );
    (nonce, message_hash)
}

/// `next_nonce(dst_chain_id)` — lazily initializes the lane to 0.
public fun next_nonce(outbox: &mut Outbox, dst_chain_id: u32): u64 {
    if (!outbox.next_nonce.contains(dst_chain_id)) {
        outbox.next_nonce.add(dst_chain_id, 0);
    };
    *outbox.next_nonce.borrow(dst_chain_id)
}

public fun is_paused(outbox: &Outbox): bool { outbox.paused }
public fun src_chain_id(outbox: &Outbox): u32 { outbox.src_chain_id }

/// Global outbound circuit breaker (§2.7). Guardian-gated.
public fun set_paused(_: &GuardianCap, outbox: &mut Outbox, paused: bool) {
    outbox.paused = paused;
    events::emit_outbox_paused(object::id(outbox), paused);
}
