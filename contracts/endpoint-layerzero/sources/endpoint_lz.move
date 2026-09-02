/// LayerZero V2 transport for the multichain vault (plan §2.2, primary
/// transport). A separate package, mirroring the oracle-adapter pattern:
/// `vault_v2` core stays dependency-light, and the transport can be
/// published/upgraded independently.
///
/// Inbound: the LayerZero executor's PTB routes the endpoint's
/// `Call<LzReceiveParam, Void>` here; `oapp::lz_receive` enforces the
/// endpoint caller and the per-eid peer pin, this module additionally
/// enforces the eid ↔ protocol-chain-id mapping, then hands the raw
/// message bytes to `vault_v2::endpoint::deliver` under the
/// `LzEndpoint` witness — `multichain` does the rest (lane, seq,
/// handlers).
///
/// Outbound: the caller's PTB takes the handler's `OutboundMessage`
/// hot potato through `send` (witness-gated consumption + eid mapping +
/// `oapp::lz_send`), routes the returned endpoint call per LayerZero's
/// PTB-builder flow, and closes with `confirm_send`.
module endpoint_lz::endpoint_lz;

use sui::coin::Coin;
use sui::sui::SUI;
use sui::table::{Self, Table};

use call::call::{Call, Void};
use call::call_cap::CallCap;
use endpoint_v2::endpoint_send::SendParam;
use endpoint_v2::endpoint_v2::EndpointV2;
use endpoint_v2::lz_receive::{Self, LzReceiveParam};
use endpoint_v2::messaging_channel::MessagingChannel;
use endpoint_v2::messaging_receipt::MessagingReceipt;
use oapp::oapp::{Self, OApp, AdminCap as LzAdminCap};
use utils::bytes32;

use options_core::admin::AdminCap;

use vault_v2::endpoint::{Self as vep, EndpointRegistry, OutboundMessage, VerifiedInbound};
use vault_v2::wire;

const E_WRONG_OAPP: u64 = 1;
const E_CHAIN_NOT_MAPPED: u64 = 2;
const E_EID_MISMATCH: u64 = 3;

/// One-time witness (also creates the OApp's package `CallCap`s).
public struct ENDPOINT_LZ has drop {}

/// The transport witness spokes bind (`bind_spoke<LzEndpoint, _>`).
/// No public constructor — only this module can instantiate it.
public struct LzEndpoint has drop {}

/// Shared transport state: the OApp capabilities plus the protocol
/// chain-id ↔ LayerZero eid mapping. The LayerZero `AdminCap` is held
/// here and exercised only through protocol-admin-gated wrappers.
public struct LzTransport has key {
    id: UID,
    cap: CallCap,
    lz_admin: LzAdminCap,
    oapp_address: address,
    eid_by_chain: Table<u64, u32>,
    chain_by_eid: Table<u32, u64>,
}

fun init(otw: ENDPOINT_LZ, ctx: &mut TxContext) {
    let (cap, lz_admin, oapp_address) = oapp::new(&otw, ctx);
    transfer::share_object(LzTransport {
        id: object::new(ctx),
        cap,
        lz_admin,
        oapp_address,
        eid_by_chain: table::new(ctx),
        chain_by_eid: table::new(ctx),
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

/// Map a protocol chain id (envelope namespace) to a LayerZero eid.
public fun map_chain(_: &AdminCap, t: &mut LzTransport, chain_id: u64, eid: u32) {
    t.eid_by_chain.add(chain_id, eid);
    t.chain_by_eid.add(eid, chain_id);
}

public fun unmap_chain(_: &AdminCap, t: &mut LzTransport, chain_id: u64) {
    let eid = t.eid_by_chain.remove(chain_id);
    t.chain_by_eid.remove(eid);
}

/// Pin the trusted peer (the spoke's `LayerZeroEndpoint.sol`, left-
/// padded to 32 bytes) for an eid, initializing the messaging channel
/// on first use. Protocol-admin-gated; the LayerZero `AdminCap` never
/// leaves the transport object.
public fun set_peer(
    _: &AdminCap,
    t: &LzTransport,
    o: &mut OApp,
    lz_endpoint: &EndpointV2,
    channel: &mut MessagingChannel,
    eid: u32,
    peer: vector<u8>,
    ctx: &mut TxContext,
) {
    assert!(object::id_address(o) == t.oapp_address, E_WRONG_OAPP);
    oapp::set_peer(o, &t.lz_admin, lz_endpoint, channel, eid, bytes32::from_bytes(peer), ctx);
}

// ══════════════════════════════ inbound ══════════════════════════════

/// Terminal leg of the LayerZero executor's delivery PTB. Peer and
/// endpoint-caller checks happen inside `oapp::lz_receive`; the wire
/// envelope's src chain must map to the delivering eid.
public fun receive(
    t: &LzTransport,
    o: &OApp,
    reg: &EndpointRegistry,
    call_obj: Call<LzReceiveParam, Void>,
    ctx: &TxContext,
): VerifiedInbound {
    assert!(object::id_address(o) == t.oapp_address, E_WRONG_OAPP);
    let param = oapp::lz_receive(o, &t.cap, call_obj);
    let (src_eid, _sender, _nonce, _guid, message, _executor, _extra, mut value) =
        lz_receive::destroy(param);
    // Executor-attached SUI has nothing to do with vault funds; hand it
    // to the transaction sender (the executor's own PTB).
    if (value.is_some()) {
        transfer::public_transfer(value.extract(), ctx.sender());
    };
    value.destroy_none();

    assert!(t.chain_by_eid.contains(src_eid), E_CHAIN_NOT_MAPPED);
    let (env, _) = wire::decode_inbound(message);
    assert!(wire::src_chain_id(&env) == *t.chain_by_eid.borrow(src_eid), E_EID_MISMATCH);
    vep::deliver(LzEndpoint {}, reg, message)
}

// ══════════════════════════════ outbound ══════════════════════════════

/// Consume a handler's `OutboundMessage` into a LayerZero send call.
/// The PTB must route the returned call through LayerZero's endpoint
/// send flow and then close it with [`confirm_send`]. `fee` comes from
/// the crank service (plan §2.4: hub-side sends are service-paid).
public fun send(
    t: &LzTransport,
    o: &mut OApp,
    reg: &EndpointRegistry,
    out: OutboundMessage,
    options: vector<u8>,
    fee: Coin<SUI>,
    ctx: &mut TxContext,
): Call<SendParam, MessagingReceipt> {
    assert!(object::id_address(o) == t.oapp_address, E_WRONG_OAPP);
    let (dst_chain_id, _dst_app, bytes) = vep::consume_outbound(LzEndpoint {}, reg, out);
    assert!(t.eid_by_chain.contains(dst_chain_id), E_CHAIN_NOT_MAPPED);
    let eid = *t.eid_by_chain.borrow(dst_chain_id);
    oapp::lz_send(
        o,
        &t.cap,
        eid,
        bytes,
        options,
        fee,
        option::none(),
        option::some(ctx.sender()),
        ctx,
    )
}

public fun confirm_send(
    t: &LzTransport,
    o: &mut OApp,
    call_obj: Call<SendParam, MessagingReceipt>,
    ctx: &TxContext,
) {
    assert!(object::id_address(o) == t.oapp_address, E_WRONG_OAPP);
    let (param, _receipt) = oapp::confirm_lz_send(o, &t.cap, call_obj);
    // Unspent fee coins come back with the param; return them to the
    // crank service that fronted them.
    let (refund_sui, mut zro_opt) = endpoint_v2::endpoint_send::destroy(param);
    transfer::public_transfer(refund_sui, ctx.sender());
    if (zro_opt.is_some()) {
        transfer::public_transfer(zro_opt.extract(), ctx.sender());
    };
    zro_opt.destroy_none();
}

// ══════════════════════════════ getters ══════════════════════════════

public fun oapp_address(t: &LzTransport): address { t.oapp_address }

public fun eid_for_chain(t: &LzTransport, chain_id: u64): u32 {
    assert!(t.eid_by_chain.contains(chain_id), E_CHAIN_NOT_MAPPED);
    *t.eid_by_chain.borrow(chain_id)
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ENDPOINT_LZ {}, ctx)
}
