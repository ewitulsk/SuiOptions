module options_rfq::events;

use sui::event;

/// Adapter-level creation event: links the option-RFQ metadata object to
/// its generic auction (which emits its own `AuctionCreated` with the
/// deadline/increment params) and the bucket it will write into.
public struct RfqCreated has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    /// Caller-supplied attribution (seller address-as-ID).
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

/// Mirrors `WriteExecuted`'s economic fields so the indexer's positions
/// materializer can treat both as "a position was minted with premium X".
public struct RfqSettled has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    call_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

/// The auction resolved without a write: no bids, or the bucket
/// expired/was invalidated mid-auction (both escrows refunded).
public struct RfqExpiredUnsold has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

public struct PutRfqCreated has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    /// Option notional in underlying units.
    amount: u64,
    /// Cash collateral escrowed = ceil(amount × strike).
    collateral: u64,
    reserve_premium: u64,
}

public struct PutRfqSettled has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    put_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
}

public struct PutRfqExpiredUnsold has copy, drop {
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

public(package) fun emit_rfq_created(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(RfqCreated { rfq_id, auction_id, bucket_id, origin, amount, reserve_premium });
}

public(package) fun emit_rfq_settled(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    call_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(RfqSettled {
        rfq_id,
        auction_id,
        bucket_id,
        origin,
        winner,
        call_recipient,
        position_id,
        position_recipient,
        amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
    });
}

public(package) fun emit_rfq_expired_unsold(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(RfqExpiredUnsold {
        rfq_id,
        auction_id,
        bucket_id,
        origin,
        amount,
        reserve_premium,
    });
}

public(package) fun emit_put_rfq_created(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    collateral: u64,
    reserve_premium: u64,
) {
    event::emit(PutRfqCreated {
        rfq_id,
        auction_id,
        bucket_id,
        origin,
        amount,
        collateral,
        reserve_premium,
    });
}

public(package) fun emit_put_rfq_settled(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    winner: address,
    put_recipient: address,
    position_id: ID,
    position_recipient: address,
    amount: u64,
    gross_premium: u64,
    fee: u64,
    net_premium: u64,
    range_start: u128,
    range_end: u128,
) {
    event::emit(PutRfqSettled {
        rfq_id,
        auction_id,
        bucket_id,
        origin,
        winner,
        put_recipient,
        position_id,
        position_recipient,
        amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
    });
}

public(package) fun emit_put_rfq_expired_unsold(
    rfq_id: ID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
) {
    event::emit(PutRfqExpiredUnsold {
        rfq_id,
        auction_id,
        bucket_id,
        origin,
        amount,
        reserve_premium,
    });
}
