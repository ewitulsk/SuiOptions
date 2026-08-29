/// Dev/CI transport (multichain plan §2.2): a registered-sender gate for
/// environments where neither LayerZero nor CCIP exists (localnet, unit
/// tests, CI). NEVER bound to a production spoke — the trust model is
/// exactly "the relayer account said so". Production lanes use the
/// LayerZero / CCIP endpoint packages.
module vault_v2::endpoint_relayer;

use vault_v2::endpoint::{Self, EndpointRegistry, OutboundMessage, VerifiedInbound};

/// The transport witness; its `TypeName` is what spokes bind.
public struct RelayerEndpoint has drop {}

/// Deliver a spoke→hub message: sender must be a registered relayer.
public fun deliver(
    reg: &EndpointRegistry,
    bytes: vector<u8>,
    ctx: &TxContext,
): VerifiedInbound {
    endpoint::assert_relayer(reg, ctx.sender());
    endpoint::deliver(RelayerEndpoint {}, reg, bytes)
}

/// Ship a hub→spoke message: for this transport, shipping IS the
/// `OutboundMessage` event `consume_outbound` emits — the relayer
/// service watches it and submits the bytes to the spoke's
/// `RelayerEndpoint.sol`.
public fun send(reg: &EndpointRegistry, out: OutboundMessage, ctx: &TxContext) {
    endpoint::assert_relayer(reg, ctx.sender());
    let (_, _, _) = endpoint::consume_outbound(RelayerEndpoint {}, reg, out);
}
