/// Chainlink CCIP transport for the multichain vault (plan §2.2,
/// secondary/standby transport). A separate package, mirroring the
/// oracle-adapter pattern.
///
/// Inbound: the CCIP OffRamp executes this package's `ccip_receive`
/// with an `Any2SuiMessage` (this package must be registered in the
/// receiver registry first — see `register_receiver`). The message's
/// source chain selector and sender are checked against the configured
/// lane, then the raw bytes go to `vault_v2::endpoint::deliver` under
/// the `CcipEndpoint` witness.
///
/// Outbound: `send` consumes a handler's `OutboundMessage` (witness-
/// gated), maps the protocol chain id to a CCIP chain selector, and
/// calls the OnRamp's `ccip_send` with the remote endpoint contract as
/// receiver and no token legs.
module endpoint_ccip::endpoint_ccip;

use sui::coin::{Coin, CoinMetadata};
use sui::clock::Clock;
use sui::dynamic_field as df;
use sui::package::{Self, Publisher};
use sui::table::{Self, Table};

use ccip::client::Any2SuiMessage;
use ccip::offramp_state_helper as osh;
use ccip::onramp_state_helper as onramp_sh;
use ccip::publisher_wrapper;
use ccip::receiver_registry;
use ccip::state_object::CCIPObjectRef;
use ccip_onramp::onramp::{Self, OnRampState};

use options_core::admin::AdminCap;

use vault_v2::endpoint::{Self as vep, EndpointRegistry, OutboundMessage, VerifiedInbound};
use vault_v2::wire;

const E_CHAIN_NOT_MAPPED: u64 = 1;
const E_SENDER_MISMATCH: u64 = 2;
const E_UNEXPECTED_TOKENS: u64 = 3;
const E_CHAIN_MISMATCH: u64 = 4;

/// One-time witness for package publish.
public struct ENDPOINT_CCIP has drop {}

/// The transport witness spokes bind (`bind_spoke<CcipEndpoint, _>`).
public struct CcipEndpoint has drop {}

/// Type proof for the CCIP receiver registry.
public struct CcipEndpointProof has drop {}

public struct PublisherKey has copy, drop, store {}

/// Shared transport state: chain-id ↔ chain-selector mapping and the
/// trusted remote endpoint contract per selector (the spoke's
/// `CCIPEndpoint.sol`, raw bytes as CCIP encodes senders).
public struct CcipTransport has key {
    id: UID,
    selector_by_chain: Table<u64, u64>,
    chain_by_selector: Table<u64, u64>,
    remote_endpoint_by_selector: Table<u64, vector<u8>>,
}

fun init(otw: ENDPOINT_CCIP, ctx: &mut TxContext) {
    let mut transport = CcipTransport {
        id: object::new(ctx),
        selector_by_chain: table::new(ctx),
        chain_by_selector: table::new(ctx),
        remote_endpoint_by_selector: table::new(ctx),
    };
    let publisher = package::claim(otw, ctx);
    df::add(&mut transport.id, PublisherKey {}, publisher);
    transfer::share_object(transport);
}

// ═══════════════════════════════ admin ═══════════════════════════════

/// Register this package in the CCIP receiver registry. MUST run before
/// any lane sends to this receiver (an unregistered receiver's messages
/// are marked SUCCESS with no retry — see Chainlink's receiver guide).
public fun register_receiver(_: &AdminCap, t: &CcipTransport, ref: &mut CCIPObjectRef) {
    let publisher: &Publisher = df::borrow(&t.id, PublisherKey {});
    let wrapper = publisher_wrapper::create(publisher, CcipEndpointProof {});
    receiver_registry::register_receiver(ref, wrapper, CcipEndpointProof {});
}

/// Map a protocol chain id to a CCIP chain selector and pin the remote
/// endpoint contract (sender AND receiver on that chain).
public fun map_chain(
    _: &AdminCap,
    t: &mut CcipTransport,
    chain_id: u64,
    selector: u64,
    remote_endpoint: vector<u8>,
) {
    t.selector_by_chain.add(chain_id, selector);
    t.chain_by_selector.add(selector, chain_id);
    t.remote_endpoint_by_selector.add(selector, remote_endpoint);
}

public fun unmap_chain(_: &AdminCap, t: &mut CcipTransport, chain_id: u64) {
    let selector = t.selector_by_chain.remove(chain_id);
    t.chain_by_selector.remove(selector);
    t.remote_endpoint_by_selector.remove(selector);
}

// ══════════════════════════════ inbound ══════════════════════════════

/// CCIP OffRamp entrypoint: consume the message, verify the lane, and
/// produce the `VerifiedInbound` hot potato for the multichain handler
/// later in the same PTB.
public fun ccip_receive(
    t: &CcipTransport,
    reg: &EndpointRegistry,
    ref: &CCIPObjectRef,
    message: Any2SuiMessage,
): VerifiedInbound {
    let (
        _message_id,
        source_chain_selector,
        sender,
        data,
        _message_receiver,
        _token_receiver,
        dest_token_amounts,
    ) = osh::consume_any2sui_message(ref, message, CcipEndpointProof {});
    // Pure messaging lane: a token leg here is misuse.
    assert!(dest_token_amounts.is_empty(), E_UNEXPECTED_TOKENS);
    assert!(t.chain_by_selector.contains(source_chain_selector), E_CHAIN_NOT_MAPPED);
    assert!(
        *t.remote_endpoint_by_selector.borrow(source_chain_selector) == sender,
        E_SENDER_MISMATCH,
    );
    let (env, _) = wire::decode_inbound(data);
    assert!(
        wire::src_chain_id(&env) == *t.chain_by_selector.borrow(source_chain_selector),
        E_CHAIN_MISMATCH,
    );
    vep::deliver(CcipEndpoint {}, reg, data)
}

// ══════════════════════════════ outbound ══════════════════════════════

/// Consume a handler's `OutboundMessage` into a CCIP send. `T` is the
/// fee token (SUI or LINK per lane config); fees come from the crank
/// service (plan §2.4: hub-side sends are service-paid). Returns the
/// CCIP message id.
public fun send<T>(
    t: &CcipTransport,
    reg: &EndpointRegistry,
    ref: &mut CCIPObjectRef,
    onramp_state: &mut OnRampState,
    clock: &Clock,
    out: OutboundMessage,
    fee_token_metadata: &CoinMetadata<T>,
    fee_token: &mut Coin<T>,
    extra_args: vector<u8>,
    ctx: &mut TxContext,
): vector<u8> {
    let (dst_chain_id, _dst_app, bytes) = vep::consume_outbound(CcipEndpoint {}, reg, out);
    assert!(t.selector_by_chain.contains(dst_chain_id), E_CHAIN_NOT_MAPPED);
    let selector = *t.selector_by_chain.borrow(dst_chain_id);
    let receiver = *t.remote_endpoint_by_selector.borrow(selector);
    onramp::ccip_send<T>(
        ref,
        onramp_state,
        clock,
        selector,
        receiver,
        bytes,
        onramp_sh::create_token_transfer_params(vector[]),
        fee_token_metadata,
        fee_token,
        extra_args,
        ctx,
    )
}

// ══════════════════════════════ getters ══════════════════════════════

public fun selector_for_chain(t: &CcipTransport, chain_id: u64): u64 {
    assert!(t.selector_by_chain.contains(chain_id), E_CHAIN_NOT_MAPPED);
    *t.selector_by_chain.borrow(chain_id)
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ENDPOINT_CCIP {}, ctx)
}
