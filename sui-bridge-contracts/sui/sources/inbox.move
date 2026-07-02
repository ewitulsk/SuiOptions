/// Inbox (one per chain): verifies an aggregated threshold signature against
/// the registered group key, enforces exactly-once delivery, and hands the
/// payload to the destination app (bridge-spec.md §2.5).
///
/// ## Dispatch model (why two steps)
///
/// Move has no dynamic dispatch, so the Inbox cannot itself call an arbitrary
/// destination package. Delivery is therefore a hot-potato handshake:
///
///   1. `receive(...)` verifies signature + dst-chain + dedup and returns a
///      `DeliveredMessage` that has NO abilities — it cannot be dropped,
///      stored, or copied.
///   2. The destination app discharges it with `consume(..., app: &UID)`,
///      proving its identity by passing a `&UID` whose 32-byte id equals
///      `dst_app`. Only the module that defines that object can produce the
///      reference, so a relayer cannot call `consume` itself.
///
/// Replay-marking happens in `consume`, atomically with delivery. An untrusted
/// relayer thus cannot consume a message without actually delivering it (the
/// whole PTB aborts and nothing is marked), closing the consume-and-drop grief.
///
/// No cross-message ordering is enforced (§2.6): the `consumed` hash-set is the
/// sole exactly-once guard; per-source nonce is tracked for observability only.
module sui_bridge::inbox;

use sui::table::{Self, Table};
use sui_bridge::envelope::{Self, SignatureEnvelope};
use sui_bridge::errors;
use sui_bridge::events;
use sui_bridge::message::{Self, CrossChainMessage};
use sui_bridge::registry::{GovernanceCap, GuardianCap, GroupKeyRegistry};

public struct Inbox has key {
    id: UID,
    /// Internal registry id of THIS chain; every accepted message must target it.
    dst_chain_id: u32,
    /// Digest domain separator (spec §2.2), derived from the deployment salt.
    domain_sep: vector<u8>,
    /// Exactly-once guard, keyed by the 32-byte canonical message hash.
    consumed: Table<vector<u8>, bool>,
    /// Highest nonce observed per source chain (observability only).
    highest_nonce: Table<u32, u64>,
    paused: bool,
}

/// Authenticated, verified message awaiting delivery. No abilities: the only
/// way to discharge it is `consume`, which records replay protection.
public struct DeliveredMessage {
    src_chain_id: u32,
    src_app: vector<u8>,
    dst_app: vector<u8>,
    nonce: u64,
    payload: vector<u8>,
    message_hash: vector<u8>,
}

/// Create + share an Inbox bound to this chain's internal id. Governance-gated.
/// `deployment_salt` is the 32-byte per-deployment salt; the digest separator is
/// derived + stored so it is auditable on-chain.
public fun create(
    _: &GovernanceCap,
    dst_chain_id: u32,
    deployment_salt: vector<u8>,
    ctx: &mut TxContext,
): ID {
    let inbox = Inbox {
        id: object::new(ctx),
        dst_chain_id,
        domain_sep: message::derive_domain_sep(deployment_salt),
        consumed: table::new(ctx),
        highest_nonce: table::new(ctx),
        paused: false,
    };
    let id = object::id(&inbox);
    transfer::share_object(inbox);
    id
}

/// Verify a message + signature envelope and return a `DeliveredMessage` for
/// the destination app to consume. Does NOT mutate the Inbox — replay-marking
/// happens in `consume`. Aborts on pause, wrong dst chain, already-consumed,
/// or an invalid/unknown-key signature.
public fun receive(
    inbox: &Inbox,
    keys: &GroupKeyRegistry,
    message: CrossChainMessage,
    envelope: SignatureEnvelope,
): DeliveredMessage {
    assert!(!inbox.paused, errors::inbox_paused());
    assert!(message::dst_chain_id(&message) == inbox.dst_chain_id, errors::wrong_dst_chain());

    let message_hash = message::hash(&message, inbox.domain_sep);
    assert!(!inbox.consumed.contains(message_hash), errors::message_already_consumed());

    envelope::verify(keys, &envelope, &message_hash);

    DeliveredMessage {
        src_chain_id: message::src_chain_id(&message),
        src_app: message::src_app(&message),
        dst_app: message::dst_app(&message),
        nonce: message::nonce(&message),
        payload: message::payload(&message),
        message_hash,
    }
}

/// Discharge a `DeliveredMessage`: prove app identity via `app` (its id must
/// equal `dst_app`), mark the message consumed, and return
/// `(src_chain_id, src_app, payload)` to the caller. The app is responsible for
/// checking `src_app` against its registered peer before acting on `payload`.
public fun consume(
    inbox: &mut Inbox,
    delivered: DeliveredMessage,
    app: &UID,
): (u32, vector<u8>, vector<u8>) {
    let DeliveredMessage { src_chain_id, src_app, dst_app, nonce, payload, message_hash } = delivered;

    assert!(object::uid_to_bytes(app) == dst_app, errors::dst_app_mismatch());
    assert!(!inbox.consumed.contains(message_hash), errors::message_already_consumed());

    inbox.consumed.add(message_hash, true);

    if (inbox.highest_nonce.contains(src_chain_id)) {
        let cur = inbox.highest_nonce.borrow_mut(src_chain_id);
        if (nonce > *cur) { *cur = nonce; };
    } else {
        inbox.highest_nonce.add(src_chain_id, nonce);
    };

    events::emit_message_delivered(object::id(inbox), message_hash, src_chain_id, nonce);
    (src_chain_id, src_app, payload)
}

// --- DeliveredMessage read-only accessors (inspect before consuming) ---
public fun delivered_src_chain_id(d: &DeliveredMessage): u32 { d.src_chain_id }
public fun delivered_src_app(d: &DeliveredMessage): vector<u8> { d.src_app }
public fun delivered_dst_app(d: &DeliveredMessage): vector<u8> { d.dst_app }
public fun delivered_nonce(d: &DeliveredMessage): u64 { d.nonce }
public fun delivered_payload(d: &DeliveredMessage): vector<u8> { d.payload }
public fun delivered_message_hash(d: &DeliveredMessage): vector<u8> { d.message_hash }

// --- views / admin ---
public fun is_consumed(inbox: &Inbox, message_hash: vector<u8>): bool {
    inbox.consumed.contains(message_hash)
}

public fun is_paused(inbox: &Inbox): bool { inbox.paused }
public fun dst_chain_id(inbox: &Inbox): u32 { inbox.dst_chain_id }

/// Global inbound circuit breaker (§2.7). Guardian-gated.
public fun set_paused(_: &GuardianCap, inbox: &mut Inbox, paused: bool) {
    inbox.paused = paused;
    events::emit_inbox_paused(object::id(inbox), paused);
}

/// Build a `DeliveredMessage` straight from a message, bypassing signature
/// verification. Lets `consume` (which needs `dst_app` to equal a runtime
/// object id) be exercised without an offline signature over that id.
#[test_only]
public fun deliver_for_testing(
    message: &CrossChainMessage,
    domain_sep: vector<u8>,
): DeliveredMessage {
    DeliveredMessage {
        src_chain_id: message::src_chain_id(message),
        src_app: message::src_app(message),
        dst_app: message::dst_app(message),
        nonce: message::nonce(message),
        payload: message::payload(message),
        message_hash: message::hash(message, domain_sep),
    }
}

/// Discharge a `DeliveredMessage` without consuming it (test cleanup for the
/// `receive` path, whose vectors use a synthetic `dst_app`).
#[test_only]
public fun destroy_delivered_for_testing(d: DeliveredMessage) {
    let DeliveredMessage {
        src_chain_id: _,
        src_app: _,
        dst_app: _,
        nonce: _,
        payload: _,
        message_hash: _,
    } = d;
}
