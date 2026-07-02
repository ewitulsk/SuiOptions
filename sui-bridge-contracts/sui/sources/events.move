/// Emitted events for the Layer 1 messaging package. Kept in one module per the
/// `options_protocol::events` convention so the indexer has a single source.
module sui_bridge::events;

use sui::event;

/// Emitted by `outbox::send`. Carries every canonical `CrossChainMessage`
/// field so the signer group can reconstruct the exact preimage off-chain.
public struct MessageCommitted has copy, drop {
    outbox_id: ID,
    message_hash: vector<u8>,
    src_chain_id: u32,
    dst_chain_id: u32,
    nonce: u64,
    src_app: vector<u8>,
    dst_app: vector<u8>,
    payload: vector<u8>,
}

/// Emitted by `inbox::consume` once a message is verified and delivered.
public struct MessageDelivered has copy, drop {
    inbox_id: ID,
    message_hash: vector<u8>,
    src_chain_id: u32,
    nonce: u64,
}

public struct OutboxPaused has copy, drop { outbox_id: ID, paused: bool }
public struct InboxPaused has copy, drop { inbox_id: ID, paused: bool }

public struct ChainRegistered has copy, drop {
    internal_id: u32,
    family: u8,
}

public struct GroupKeyRegistered has copy, drop {
    group_pubkey_id: u32,
    scheme_tag: u8,
}

public(package) fun emit_message_committed(
    outbox_id: ID,
    message_hash: vector<u8>,
    src_chain_id: u32,
    dst_chain_id: u32,
    nonce: u64,
    src_app: vector<u8>,
    dst_app: vector<u8>,
    payload: vector<u8>,
) {
    event::emit(MessageCommitted {
        outbox_id,
        message_hash,
        src_chain_id,
        dst_chain_id,
        nonce,
        src_app,
        dst_app,
        payload,
    });
}

public(package) fun emit_message_delivered(
    inbox_id: ID,
    message_hash: vector<u8>,
    src_chain_id: u32,
    nonce: u64,
) {
    event::emit(MessageDelivered { inbox_id, message_hash, src_chain_id, nonce });
}

public(package) fun emit_outbox_paused(outbox_id: ID, paused: bool) {
    event::emit(OutboxPaused { outbox_id, paused });
}

public(package) fun emit_inbox_paused(inbox_id: ID, paused: bool) {
    event::emit(InboxPaused { inbox_id, paused });
}

public(package) fun emit_chain_registered(internal_id: u32, family: u8) {
    event::emit(ChainRegistered { internal_id, family });
}

public(package) fun emit_group_key_registered(group_pubkey_id: u32, scheme_tag: u8) {
    event::emit(GroupKeyRegistered { group_pubkey_id, scheme_tag });
}
