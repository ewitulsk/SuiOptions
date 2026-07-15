module auction::events;

use std::type_name::TypeName;
use sui::event;

/// One event set for every auction regardless of asset pair or coupling;
/// `escrow_type` / `bid_type` carry the legs for indexing.
public struct AuctionCreated has copy, drop {
    auction_id: ID,
    origin: ID,
    escrow_type: TypeName,
    bid_type: TypeName,
    amount: u64,
    reserve_bid: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
    coupled: bool,
}

public struct AuctionBid has copy, drop {
    auction_id: ID,
    bidder: address,
    token_recipient: address,
    amount: u64,
    previous_best: u64,
    deadline_ms: u64,
}

/// Emitted by the uncoupled `settle` path. Coupled venues emit their own
/// settlement events (they know what the legs mean).
public struct AuctionSettled has copy, drop {
    auction_id: ID,
    origin: ID,
    bidder: address,
    token_recipient: address,
    amount: u64,
    winning_bid: u64,
}

public struct AuctionUnfilled has copy, drop {
    auction_id: ID,
    origin: ID,
    amount: u64,
    reserve_bid: u64,
}

public(package) fun emit_auction_created(
    auction_id: ID,
    origin: ID,
    escrow_type: TypeName,
    bid_type: TypeName,
    amount: u64,
    reserve_bid: u64,
    deadline_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
    coupled: bool,
) {
    event::emit(AuctionCreated {
        auction_id,
        origin,
        escrow_type,
        bid_type,
        amount,
        reserve_bid,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
        coupled,
    });
}

public(package) fun emit_auction_bid(
    auction_id: ID,
    bidder: address,
    token_recipient: address,
    amount: u64,
    previous_best: u64,
    deadline_ms: u64,
) {
    event::emit(AuctionBid {
        auction_id,
        bidder,
        token_recipient,
        amount,
        previous_best,
        deadline_ms,
    });
}

public(package) fun emit_auction_settled(
    auction_id: ID,
    origin: ID,
    bidder: address,
    token_recipient: address,
    amount: u64,
    winning_bid: u64,
) {
    event::emit(AuctionSettled {
        auction_id,
        origin,
        bidder,
        token_recipient,
        amount,
        winning_bid,
    });
}

public(package) fun emit_auction_unfilled(
    auction_id: ID,
    origin: ID,
    amount: u64,
    reserve_bid: u64,
) {
    event::emit(AuctionUnfilled { auction_id, origin, amount, reserve_bid });
}
