/// Transport-agnostic messaging endpoint layer (multichain plan §2.2).
///
/// Each concrete transport (LayerZero, CCIP, the dev relayer) is a
/// witness-typed module allow-listed in the shared `EndpointRegistry`.
/// A transport verifies delivery through its own stack, then constructs
/// a `VerifiedInbound` hot potato here; only `vault_v2::multichain` can
/// open it, and it checks the spoke binding (endpoint type + lane +
/// sequence) before applying the payload. Outbound, handlers return an
/// `OutboundMessage` hot potato that only the spoke's bound transport
/// witness can consume in the same PTB.
///
/// Trust note: this layer authenticates WHICH transport delivered a
/// message, not its content — content authenticity is the transport's
/// job (LayerZero DVNs / Chainlink DON / the dev relayer's sender gate).
module vault_v2::endpoint;

use std::type_name::{Self, TypeName};
use sui::vec_set::{Self, VecSet};

use options_core::admin::AdminCap;

use vault_v2::errors;
use vault_v2::events;
use vault_v2::wire::{Self, Envelope, Inbound};

/// Shared transport governance: which endpoint witnesses may deliver,
/// which senders the dev relayer endpoint accepts, and this
/// deployment's protocol chain id (envelope `dst_chain_id` on inbound,
/// `src_chain_id` on outbound).
public struct EndpointRegistry has key {
    id: UID,
    allowed: VecSet<TypeName>,
    relayers: VecSet<address>,
    /// 0 = unset; every lane check aborts until the admin seeds it.
    hub_chain_id: u64,
}

/// Hot potato: a transport-verified, wire-decoded inbound message.
public struct VerifiedInbound {
    endpoint: TypeName,
    envelope: Envelope,
    inbound: Inbound,
}

/// Hot potato: a fully-encoded hub→spoke message that only the named
/// endpoint witness may consume (send or emit) this transaction.
public struct OutboundMessage {
    endpoint: TypeName,
    dst_chain_id: u64,
    dst_app: address,
    seq: u64,
    msg_type: u8,
    bytes: vector<u8>,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(EndpointRegistry {
        id: object::new(ctx),
        allowed: vec_set::empty(),
        relayers: vec_set::empty(),
        hub_chain_id: 0,
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun allow_endpoint<W>(_: &AdminCap, reg: &mut EndpointRegistry) {
    reg.allowed.insert(type_name::with_defining_ids<W>());
    events::emit_endpoint_allowed(type_name::with_defining_ids<W>());
}

public fun disallow_endpoint<W>(_: &AdminCap, reg: &mut EndpointRegistry) {
    reg.allowed.remove(&type_name::with_defining_ids<W>());
    events::emit_endpoint_disallowed(type_name::with_defining_ids<W>());
}

public fun add_relayer(_: &AdminCap, reg: &mut EndpointRegistry, relayer: address) {
    reg.relayers.insert(relayer);
    events::emit_relayer_added(relayer);
}

public fun remove_relayer(_: &AdminCap, reg: &mut EndpointRegistry, relayer: address) {
    reg.relayers.remove(&relayer);
    events::emit_relayer_removed(relayer);
}

public fun set_hub_chain_id(_: &AdminCap, reg: &mut EndpointRegistry, chain_id: u64) {
    assert!(chain_id != 0, errors::config_invalid());
    reg.hub_chain_id = chain_id;
}

// ══════════════════════════ transport surface ══════════════════════════

/// Transport modules call this AFTER their own delivery verification.
public fun deliver<W: drop>(
    _witness: W,
    reg: &EndpointRegistry,
    bytes: vector<u8>,
): VerifiedInbound {
    let endpoint = type_name::with_defining_ids<W>();
    assert!(reg.allowed.contains(&endpoint), errors::endpoint_not_allowed());
    let (envelope, inbound) = wire::decode_inbound(bytes);
    VerifiedInbound { endpoint, envelope, inbound }
}

/// The spoke's bound transport consumes the outbound for shipping.
/// Returns `(dst_chain_id, dst_app, bytes)`.
public fun consume_outbound<W: drop>(
    _witness: W,
    reg: &EndpointRegistry,
    out: OutboundMessage,
): (u64, address, vector<u8>) {
    let OutboundMessage { endpoint, dst_chain_id, dst_app, seq, msg_type, bytes } = out;
    let w = type_name::with_defining_ids<W>();
    assert!(w == endpoint, errors::wrong_endpoint());
    assert!(reg.allowed.contains(&w), errors::endpoint_not_allowed());
    events::emit_outbound_message(endpoint, dst_chain_id, dst_app, seq, msg_type, bytes);
    (dst_chain_id, dst_app, bytes)
}

/// Dev-relayer sender gate, used by `endpoint_relayer`.
public fun assert_relayer(reg: &EndpointRegistry, sender: address) {
    assert!(reg.relayers.contains(&sender), errors::relayer_not_allowed());
}

public fun hub_chain_id(reg: &EndpointRegistry): u64 {
    assert!(reg.hub_chain_id != 0, errors::hub_chain_unset());
    reg.hub_chain_id
}

public fun is_endpoint_allowed(reg: &EndpointRegistry, t: &TypeName): bool {
    reg.allowed.contains(t)
}

// ══════════════════════ package-internal surface ══════════════════════

public(package) fun open(v: VerifiedInbound): (TypeName, Envelope, Inbound) {
    let VerifiedInbound { endpoint, envelope, inbound } = v;
    (endpoint, envelope, inbound)
}

public(package) fun make_outbound(
    endpoint: TypeName,
    dst_chain_id: u64,
    dst_app: address,
    seq: u64,
    msg_type: u8,
    bytes: vector<u8>,
): OutboundMessage {
    OutboundMessage { endpoint, dst_chain_id, dst_app, seq, msg_type, bytes }
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) { init(ctx) }
