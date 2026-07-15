/// Generic escrowed ascending auction — the unification of the options
/// protocol's `rfq`, `rfq_put` and `swap_auction` venues (and the Sui
/// mirror of the Solana port's `auction_venue` program): one machine with
/// escrowed bids, a reserve floor, a strict minimum increment, anti-snipe
/// deadline extension, and a permissionless settle.
///
/// The seller escrows `Escrow` and opens the auction; bidders escrow
/// `Bid`-asset bids on-chain; after the deadline anyone settles. Escrowed
/// bids are what make the best bid *always* settleable, which is what
/// makes the settle crank permissionless.
///
/// Coupling. An uncoupled auction (public `create`) is a pure swap:
/// escrow → winner, winning bid → proceeds recipient, all routed by the
/// public `settle`. A *coupled* auction (`create_coupled<W>`) records the
/// witness type `W` as its settle authority: only a caller able to mint
/// `W` may `finalize` it, receiving the raw balances as hot potatoes to
/// absorb in its own transaction. This is how downstream venues (the
/// options RFQ adapters, the covered-call vault) gate settlement and
/// routing without this package knowing anything about them.
module auction::auction;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use auction::errors;
use auction::events;

/// Minimum auction duration, so bidders can react to `AuctionCreated`.
const MIN_DURATION_MS: u64 = 300_000; // 5 minutes

const BPS_DENOM: u128 = 10_000;

public struct Auction<phantom Escrow, phantom Bid> has key {
    id: UID,
    /// The asset being sold; handed to the settle path when the auction
    /// resolves.
    escrow: Balance<Escrow>,
    /// == escrow.value(), cached for reads.
    amount: u64,
    /// Bids below this are rejected — the only price-safety floor a quiet
    /// auction has. The creator derives it (a coupled vault from Pyth; a
    /// standalone seller however they like).
    reserve_bid: u64,
    created_ms: u64,
    deadline_ms: u64,
    /// Anti-snipe: a best bid landing inside `snipe_window_ms` of the
    /// deadline pushes the deadline out by `snipe_extension_ms`, capped
    /// at `max_deadline_ms` — a last-block snipe becomes an open price
    /// war.
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_deadline_ms: u64,
    /// Minimum improvement over the current best, in bps of the best.
    min_increment_bps: u64,
    /// Current best bid; the bid itself is escrowed in `bid_escrow` (its
    /// value IS the best bid — no duplicate field to drift).
    best_bidder: Option<address>,
    /// Where the winner wants the escrow (uncoupled) or the venue-defined
    /// winnings (coupled: e.g. minted option coins) delivered.
    best_token_recipient: Option<address>,
    bid_escrow: Balance<Bid>,
    /// Uncoupled `settle` routing, fixed at creation: winning bid →
    /// `proceeds_recipient`; unfilled escrow → `refund_recipient`.
    proceeds_recipient: address,
    refund_recipient: address,
    /// Originating object (venue ID, or seller address-as-ID). Indexing
    /// and origin-gating only.
    origin: ID,
    /// `some(TypeName<W>)` for coupled auctions: only `finalize<…, W>` /
    /// `finalize_early<…, W>` may resolve it. `none` ⇒ the public
    /// `settle` path.
    settle_authority: Option<TypeName>,
}

/// The winning side of a finalized auction — a hot potato handed to the
/// settle path (this module's `settle`, or the coupled venue) to absorb
/// in the same transaction.
public struct FinalizedBid<phantom Bid> {
    bidder: address,
    token_recipient: address,
    bid: Balance<Bid>,
}

/// Identity/params of a finalized auction, for event emission by the
/// caller after the object is gone.
public struct AuctionReceipt has copy, drop {
    auction_id: ID,
    origin: ID,
    amount: u64,
    reserve_bid: u64,
}

/// Open an uncoupled auction: escrow the asset and start the clock.
/// Deliberately public and seller-agnostic — anyone can run a generic
/// swap auction; `settle` routes the outcome to addresses fixed here.
public fun create<Escrow, Bid>(
    escrow: Coin<Escrow>,
    reserve_bid: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    proceeds_recipient: address,
    refund_recipient: address,
    origin: ID,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    create_impl<Escrow, Bid>(
        escrow.into_balance(),
        reserve_bid,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        proceeds_recipient,
        refund_recipient,
        origin,
        option::none(),
        clock,
        ctx,
    )
}

/// Coupled variant: records `W` as the settle authority, so only the
/// venue module able to mint `W` can resolve the auction (via `finalize`
/// / `finalize_early`) and absorb the balances in-place. Escrow arrives
/// as a `Balance` because venue escrows live as balances.
public fun create_coupled<Escrow, Bid, W: drop>(
    _witness: W,
    escrow: Balance<Escrow>,
    reserve_bid: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    origin: ID,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let origin_addr = origin.to_address();
    create_impl<Escrow, Bid>(
        escrow,
        reserve_bid,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        origin_addr,
        origin_addr,
        origin,
        option::some(type_name::with_defining_ids<W>()),
        clock,
        ctx,
    )
}

fun create_impl<Escrow, Bid>(
    escrow: Balance<Escrow>,
    reserve_bid: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    proceeds_recipient: address,
    refund_recipient: address,
    origin: ID,
    settle_authority: Option<TypeName>,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let amount = escrow.value();
    assert!(amount > 0, errors::zero_amount());
    assert!(duration_ms >= MIN_DURATION_MS, errors::duration_too_short());
    let now = clock.timestamp_ms();
    let deadline_ms = now + duration_ms;
    let max_deadline_ms = deadline_ms + max_extension_ms;

    let auction = Auction<Escrow, Bid> {
        id: object::new(ctx),
        escrow,
        amount,
        reserve_bid,
        created_ms: now,
        deadline_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_deadline_ms,
        min_increment_bps,
        best_bidder: option::none(),
        best_token_recipient: option::none(),
        bid_escrow: balance::zero<Bid>(),
        proceeds_recipient,
        refund_recipient,
        origin,
        settle_authority,
    };
    let auction_id = object::id(&auction);
    events::emit_auction_created(
        auction_id,
        origin,
        type_name::with_defining_ids<Escrow>(),
        type_name::with_defining_ids<Bid>(),
        amount,
        reserve_bid,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
        auction.settle_authority.is_some(),
    );
    transfer::share_object(auction);
    auction_id
}

/// Escrow a bid. Must beat `max(reserve, best × (1 + increment))` and
/// strictly beat the standing best. The previous best bid is refunded by
/// push transfer — always succeeds on Sui, no re-entry, no blocking.
public fun bid<Escrow, Bid>(
    auction: &mut Auction<Escrow, Bid>,
    bid_in: Coin<Bid>,
    token_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < auction.deadline_ms, errors::auction_closed());

    let value = bid_in.value();
    let previous = auction.bid_escrow.value();
    let floor = if (auction.best_bidder.is_some()) {
        // Ceiling division so a non-zero increment always forces a real
        // improvement; the strict `>` handles min_increment_bps == 0.
        let with_increment = (
            ((previous as u128) * (BPS_DENOM + (auction.min_increment_bps as u128))
                + BPS_DENOM - 1) / BPS_DENOM
        ) as u64;
        assert!(value > previous, errors::bid_too_low());
        with_increment.max(auction.reserve_bid)
    } else {
        auction.reserve_bid
    };
    assert!(value >= floor, errors::bid_too_low());

    // Refund the outbid party, if any.
    if (auction.best_bidder.is_some()) {
        let refund = coin::from_balance(auction.bid_escrow.withdraw_all(), ctx);
        transfer::public_transfer(refund, *auction.best_bidder.borrow());
    };
    auction.bid_escrow.join(bid_in.into_balance());
    auction.best_bidder = option::some(ctx.sender());
    auction.best_token_recipient = option::some(token_recipient);

    // Anti-snipe: late best bids extend the deadline (capped).
    if (auction.deadline_ms - now < auction.snipe_window_ms) {
        let extended = now + auction.snipe_extension_ms;
        auction.deadline_ms = extended.min(auction.max_deadline_ms);
    };

    events::emit_auction_bid(
        object::id(auction),
        ctx.sender(),
        token_recipient,
        value,
        previous,
        auction.deadline_ms,
    );
}

/// Consume a closed coupled auction and hand its parts to the venue that
/// owns the settle authority. Returns `(winner, escrow, receipt)`;
/// `winner == none` ⇒ no bids and `escrow` is the full refund.
public fun finalize<Escrow, Bid, W: drop>(
    _witness: W,
    auction: Auction<Escrow, Bid>,
    clock: &Clock,
): (Option<FinalizedBid<Bid>>, Balance<Escrow>, AuctionReceipt) {
    assert!(clock.timestamp_ms() >= auction.deadline_ms, errors::auction_not_closed());
    assert_authority<Escrow, Bid, W>(&auction);
    destroy(auction)
}

/// Authority-only recovery with no deadline precondition, for when the
/// venue knows the auction is moot before its clock runs out (e.g. the
/// options adapters' bucket expired or was invalidated mid-auction). The
/// venue's own preconditions gate legitimacy; the standing bid must be
/// refunded to the bidder by the caller.
public fun finalize_early<Escrow, Bid, W: drop>(
    _witness: W,
    auction: Auction<Escrow, Bid>,
): (Option<FinalizedBid<Bid>>, Balance<Escrow>, AuctionReceipt) {
    assert_authority<Escrow, Bid, W>(&auction);
    destroy(auction)
}

fun assert_authority<Escrow, Bid, W: drop>(auction: &Auction<Escrow, Bid>) {
    assert!(
        auction.settle_authority.is_some()
            && *auction.settle_authority.borrow() == type_name::with_defining_ids<W>(),
        errors::not_settle_authority(),
    );
}

/// Shared destructure for the finalize paths and `settle`.
fun destroy<Escrow, Bid>(
    auction: Auction<Escrow, Bid>,
): (Option<FinalizedBid<Bid>>, Balance<Escrow>, AuctionReceipt) {
    let Auction {
        id,
        escrow,
        amount,
        reserve_bid,
        created_ms: _,
        deadline_ms: _,
        snipe_window_ms: _,
        snipe_extension_ms: _,
        max_deadline_ms: _,
        min_increment_bps: _,
        mut best_bidder,
        mut best_token_recipient,
        bid_escrow,
        proceeds_recipient: _,
        refund_recipient: _,
        origin,
        settle_authority: _,
    } = auction;
    let receipt = AuctionReceipt {
        auction_id: id.to_inner(),
        origin,
        amount,
        reserve_bid,
    };
    id.delete();

    let winner = if (best_bidder.is_some()) {
        option::some(FinalizedBid {
            bidder: best_bidder.extract(),
            token_recipient: best_token_recipient.extract(),
            bid: bid_escrow,
        })
    } else {
        bid_escrow.destroy_zero();
        option::none()
    };
    best_bidder.destroy_none();
    best_token_recipient.destroy_none();
    (winner, escrow, receipt)
}

/// Settle a closed *uncoupled* auction — callable by anyone; every output
/// goes to an address fixed at creation (or to the winner). Winner:
/// escrow → the winner's token recipient, bid → proceeds recipient. No
/// winner: escrow → refund recipient.
public fun settle<Escrow, Bid>(
    auction: Auction<Escrow, Bid>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(auction.settle_authority.is_none(), errors::settle_coupled());
    assert!(clock.timestamp_ms() >= auction.deadline_ms, errors::auction_not_closed());
    let proceeds_recipient = auction.proceeds_recipient;
    let refund_recipient = auction.refund_recipient;
    let (mut winner, escrow, receipt) = destroy(auction);

    if (winner.is_some()) {
        let FinalizedBid { bidder, token_recipient, bid } = winner.extract();
        winner.destroy_none();
        let winning_bid = bid.value();
        transfer::public_transfer(coin::from_balance(escrow, ctx), token_recipient);
        transfer::public_transfer(coin::from_balance(bid, ctx), proceeds_recipient);
        events::emit_auction_settled(
            receipt.auction_id,
            receipt.origin,
            bidder,
            token_recipient,
            receipt.amount,
            winning_bid,
        );
    } else {
        winner.destroy_none();
        transfer::public_transfer(coin::from_balance(escrow, ctx), refund_recipient);
        events::emit_auction_unfilled(
            receipt.auction_id,
            receipt.origin,
            receipt.amount,
            receipt.reserve_bid,
        );
    };
}

// ---- FinalizedBid / AuctionReceipt accessors (coupled venues) ----
// Public: only a witness-gated finalize can produce these values, and the
// hot potato forces same-transaction consumption.

public fun bid_bidder<B>(b: &FinalizedBid<B>): address { b.bidder }
public fun bid_token_recipient<B>(b: &FinalizedBid<B>): address { b.token_recipient }

public fun unpack_bid<B>(b: FinalizedBid<B>): (address, address, Balance<B>) {
    let FinalizedBid { bidder, token_recipient, bid } = b;
    (bidder, token_recipient, bid)
}

public fun receipt_auction_id(r: &AuctionReceipt): ID { r.auction_id }
public fun receipt_origin(r: &AuctionReceipt): ID { r.origin }
public fun receipt_amount(r: &AuctionReceipt): u64 { r.amount }
public fun receipt_reserve_bid(r: &AuctionReceipt): u64 { r.reserve_bid }

// ---- getters ----

public fun amount<E, B>(auction: &Auction<E, B>): u64 { auction.amount }
public fun reserve_bid<E, B>(auction: &Auction<E, B>): u64 { auction.reserve_bid }
public fun deadline_ms<E, B>(auction: &Auction<E, B>): u64 { auction.deadline_ms }
public fun max_deadline_ms<E, B>(auction: &Auction<E, B>): u64 { auction.max_deadline_ms }
public fun origin<E, B>(auction: &Auction<E, B>): ID { auction.origin }
public fun best_bidder<E, B>(auction: &Auction<E, B>): Option<address> { auction.best_bidder }
public fun coupled<E, B>(auction: &Auction<E, B>): bool { auction.settle_authority.is_some() }

/// The standing best bid (0 when no bids).
public fun best_bid<E, B>(auction: &Auction<E, B>): u64 {
    auction.bid_escrow.value()
}

public fun proceeds_recipient<E, B>(auction: &Auction<E, B>): address {
    auction.proceeds_recipient
}

public fun refund_recipient<E, B>(auction: &Auction<E, B>): address {
    auction.refund_recipient
}
