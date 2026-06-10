/// On-chain RFQ: an open ascending premium auction with escrowed bids
/// (docs/vault-implementation-guide/02-onchain-rfq.md).
///
/// A seller escrows underlying and opens an auction; MMs escrow premium
/// bids on-chain; after the deadline anyone settles — the contract
/// executes the covered write against the best bid. Escrowed bids are
/// what make the best bid *always* settleable, which is what makes the
/// settle crank permissionless. Each slice is an independent shared
/// object, so slices parallelize across MMs and a failed slice doesn't
/// poison the round.
module options_protocol::rfq;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use options_protocol::admin::ProtocolConfig;
use options_protocol::bucket::{Self, Bucket};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position;
use options_protocol::treasury::Treasury;

/// Guarantees the settle crank can land while the bucket still accepts
/// writes: the auction's hard deadline must precede bucket expiry by at
/// least this much.
const SETTLE_BUFFER_MS: u64 = 600_000; // 10 minutes

/// Minimum auction duration, so MMs can react to `RfqCreated`.
const MIN_DURATION_MS: u64 = 300_000; // 5 minutes

const BPS_DENOM: u128 = 10_000;

public struct RfqAuction<phantom Underlying, phantom Settlement, phantom Call> has key {
    id: UID,
    bucket_id: ID,
    /// Underlying escrowed for this slice; written into the bucket at
    /// settle.
    underlying: Balance<Underlying>,
    /// == underlying.value(), cached for reads.
    amount: u64,
    /// Total premium floor for the slice (settlement smallest-units).
    /// Bids below this are rejected — the only price-safety floor a
    /// quiet auction has (the vault derives it from Pyth, doc 03 §6).
    reserve_premium: u64,
    created_ms: u64,
    deadline_ms: u64,
    /// Anti-snipe: a best bid landing inside `snipe_window_ms` of the
    /// deadline pushes the deadline out by `snipe_extension_ms`, capped
    /// at `max_deadline_ms`.
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_deadline_ms: u64,
    /// Minimum improvement over the current best, in bps of the best.
    min_increment_bps: u64,
    /// Current best bid; the premium itself is escrowed in `bid_escrow`
    /// (its value IS the best premium — no duplicate field to drift).
    best_bidder: Option<address>,
    best_call_recipient: Option<address>,
    bid_escrow: Balance<Settlement>,
    /// Where settle() sends the outputs. The vault-coupled path
    /// (`finalize`) bypasses these and absorbs directly.
    position_recipient: address,
    proceeds_recipient: address,
    /// Originating object (vault ID, or seller address-as-ID). Indexing
    /// and origin-gating only.
    origin: ID,
}

/// The winning side of a finalized auction, handed back to a venue
/// module (the vault) to absorb in its own transaction.
public struct FinalizedBid<phantom Settlement> {
    bidder: address,
    call_recipient: address,
    premium: Balance<Settlement>,
}

/// Identity/params of a finalized auction, for event emission by the
/// caller after the object is gone.
public struct RfqReceipt has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

/// Open an auction: escrow `underlying` and start the clock. Deliberately
/// public and seller-agnostic — the vault is just one caller; any holder
/// of underlying can auction a covered write.
public fun create<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    underlying: Coin<Underlying>,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    position_recipient: address,
    proceeds_recipient: address,
    origin: ID,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let amount = underlying.value();
    assert!(amount > 0, errors::zero_amount());
    assert!(!bucket::invalidated(bucket), errors::bucket_invalidated());
    let now = clock.timestamp_ms();
    assert!(now < bucket::expiry_ms(bucket), errors::bucket_expired());
    assert!(duration_ms >= MIN_DURATION_MS, errors::rfq_duration_too_short());

    let deadline_ms = now + duration_ms;
    let max_deadline_ms = deadline_ms + max_extension_ms;
    assert!(
        max_deadline_ms + SETTLE_BUFFER_MS <= bucket::expiry_ms(bucket),
        errors::rfq_too_close_to_expiry(),
    );

    let rfq = RfqAuction<Underlying, Settlement, Call> {
        id: object::new(ctx),
        bucket_id: object::id(bucket),
        underlying: underlying.into_balance(),
        amount,
        reserve_premium,
        created_ms: now,
        deadline_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_deadline_ms,
        min_increment_bps,
        best_bidder: option::none(),
        best_call_recipient: option::none(),
        bid_escrow: balance::zero<Settlement>(),
        position_recipient,
        proceeds_recipient,
        origin,
    };
    let rfq_id = object::id(&rfq);
    events::emit_rfq_created(
        rfq_id,
        rfq.bucket_id,
        origin,
        amount,
        reserve_premium,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    );
    transfer::share_object(rfq);
    rfq_id
}

/// Escrow a bid. Must beat `max(reserve, best × (1 + increment))` and
/// strictly beat the standing best. The previous best bid is refunded by
/// push transfer — always succeeds on Sui, no re-entry, no blocking.
public fun bid<Underlying, Settlement, Call>(
    rfq: &mut RfqAuction<Underlying, Settlement, Call>,
    premium_in: Coin<Settlement>,
    call_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < rfq.deadline_ms, errors::rfq_closed());

    let value = premium_in.value();
    let previous = rfq.bid_escrow.value();
    let floor = if (rfq.best_bidder.is_some()) {
        // Ceiling division so a non-zero increment always forces a real
        // improvement; the strict `>` handles min_increment_bps == 0.
        let with_increment = (
            ((previous as u128) * (BPS_DENOM + (rfq.min_increment_bps as u128))
                + BPS_DENOM - 1) / BPS_DENOM
        ) as u64;
        assert!(value > previous, errors::rfq_bid_too_low());
        with_increment.max(rfq.reserve_premium)
    } else {
        rfq.reserve_premium
    };
    assert!(value >= floor, errors::rfq_bid_too_low());

    // Refund the outbid party, if any.
    if (rfq.best_bidder.is_some()) {
        let refund = coin::from_balance(rfq.bid_escrow.withdraw_all(), ctx);
        transfer::public_transfer(refund, *rfq.best_bidder.borrow());
    };
    rfq.bid_escrow.join(premium_in.into_balance());
    rfq.best_bidder = option::some(ctx.sender());
    rfq.best_call_recipient = option::some(call_recipient);

    // Anti-snipe: late best bids extend the deadline (capped), turning a
    // last-block snipe into an open price war.
    if (rfq.deadline_ms - now < rfq.snipe_window_ms) {
        let extended = now + rfq.snipe_extension_ms;
        rfq.deadline_ms = extended.min(rfq.max_deadline_ms);
    };

    events::emit_rfq_bid(
        object::id(rfq),
        ctx.sender(),
        call_recipient,
        value,
        previous,
        rfq.deadline_ms,
    );
}

/// Consume a closed auction and hand its parts to the calling module
/// (the vault's `settle_rfq` absorbs them directly; `settle` below wraps
/// this for everyone else, so the two paths cannot diverge).
///
/// Returns `(winner, collateral, receipt)`; `winner == none` ⇒ no bids,
/// `collateral` is the refunded underlying.
public(package) fun finalize<Underlying, Settlement, Call>(
    rfq: RfqAuction<Underlying, Settlement, Call>,
    clock: &Clock,
): (Option<FinalizedBid<Settlement>>, Balance<Underlying>, RfqReceipt) {
    assert!(clock.timestamp_ms() >= rfq.deadline_ms, errors::rfq_not_closed());
    destroy(rfq)
}

/// Shared destructure for finalize / settle_expired.
fun destroy<Underlying, Settlement, Call>(
    rfq: RfqAuction<Underlying, Settlement, Call>,
): (Option<FinalizedBid<Settlement>>, Balance<Underlying>, RfqReceipt) {
    let RfqAuction {
        id,
        bucket_id,
        underlying,
        amount,
        reserve_premium,
        created_ms: _,
        deadline_ms: _,
        snipe_window_ms: _,
        snipe_extension_ms: _,
        max_deadline_ms: _,
        min_increment_bps: _,
        mut best_bidder,
        mut best_call_recipient,
        bid_escrow,
        position_recipient: _,
        proceeds_recipient: _,
        origin,
    } = rfq;
    let receipt = RfqReceipt {
        rfq_id: id.to_inner(),
        bucket_id,
        origin,
        amount,
        reserve_premium,
    };
    id.delete();

    let winner = if (best_bidder.is_some()) {
        option::some(FinalizedBid {
            bidder: best_bidder.extract(),
            call_recipient: best_call_recipient.extract(),
            premium: bid_escrow,
        })
    } else {
        bid_escrow.destroy_zero();
        option::none()
    };
    best_bidder.destroy_none();
    best_call_recipient.destroy_none();
    (winner, underlying, receipt)
}

/// Settle a closed auction — callable by anyone; every output goes to an
/// address fixed at creation (or to the winner). Winner: protocol fee is
/// skimmed, the covered write executes, the winner gets `Coin<Call>`, the
/// seller gets the `Position` + net premium. No winner: collateral is
/// refunded.
public fun settle<Underlying, Settlement, Call>(
    rfq: RfqAuction<Underlying, Settlement, Call>,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    let position_recipient = rfq.position_recipient;
    let proceeds_recipient = rfq.proceeds_recipient;
    let (mut winner, underlying, receipt) = finalize(rfq, clock);

    if (winner.is_some()) {
        let FinalizedBid { bidder, call_recipient, premium } = winner.extract();
        winner.destroy_none();
        let gross_premium = premium.value();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();

        let (pos, call) =
            bucket::write_collateralized_balance(bucket, underlying, clock, ctx);
        let position_id = object::id(&pos);
        let range_start = position::range_start(&pos);
        let range_end = position::range_end(&pos);
        transfer::public_transfer(call, call_recipient);
        transfer::public_transfer(pos, position_recipient);
        transfer::public_transfer(coin::from_balance(net, ctx), proceeds_recipient);

        events::emit_rfq_settled(
            receipt.rfq_id,
            receipt.bucket_id,
            receipt.origin,
            bidder,
            call_recipient,
            position_id,
            position_recipient,
            receipt.amount,
            gross_premium,
            fee,
            net_premium,
            range_start,
            range_end,
        );
    } else {
        winner.destroy_none();
        transfer::public_transfer(coin::from_balance(underlying, ctx), proceeds_recipient);
        events::emit_rfq_expired_unsold(
            receipt.rfq_id,
            receipt.bucket_id,
            receipt.origin,
            receipt.amount,
            receipt.reserve_premium,
        );
    };
}

/// Recovery path: the bucket expired or was invalidated mid-auction, so
/// the write can never execute — refund both escrows (bid → bidder,
/// underlying → proceeds recipient) so funds can never be stranded.
/// Callable by anyone, with no deadline precondition: once the bucket is
/// dead the auction is moot.
public fun settle_expired<Underlying, Settlement, Call>(
    rfq: RfqAuction<Underlying, Settlement, Call>,
    bucket: &Bucket<Underlying, Settlement, Call>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    assert!(
        clock.timestamp_ms() >= bucket::expiry_ms(bucket) || bucket::invalidated(bucket),
        errors::bucket_not_expired(),
    );
    let proceeds_recipient = rfq.proceeds_recipient;
    let (mut winner, underlying, receipt) = destroy(rfq);

    if (winner.is_some()) {
        let FinalizedBid { bidder, call_recipient: _, premium } = winner.extract();
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    transfer::public_transfer(coin::from_balance(underlying, ctx), proceeds_recipient);
    events::emit_rfq_expired_unsold(
        receipt.rfq_id,
        receipt.bucket_id,
        receipt.origin,
        receipt.amount,
        receipt.reserve_premium,
    );
}

// ---- FinalizedBid / RfqReceipt accessors (vault settle path) ----

public(package) fun bid_bidder<S>(b: &FinalizedBid<S>): address { b.bidder }
public(package) fun bid_call_recipient<S>(b: &FinalizedBid<S>): address { b.call_recipient }

public(package) fun unpack_bid<S>(b: FinalizedBid<S>): (address, address, Balance<S>) {
    let FinalizedBid { bidder, call_recipient, premium } = b;
    (bidder, call_recipient, premium)
}

public(package) fun receipt_rfq_id(r: &RfqReceipt): ID { r.rfq_id }
public(package) fun receipt_bucket_id(r: &RfqReceipt): ID { r.bucket_id }
public(package) fun receipt_origin(r: &RfqReceipt): ID { r.origin }
public(package) fun receipt_amount(r: &RfqReceipt): u64 { r.amount }
public(package) fun receipt_reserve_premium(r: &RfqReceipt): u64 { r.reserve_premium }

// ---- getters ----

public fun bucket_id<U, S, C>(rfq: &RfqAuction<U, S, C>): ID { rfq.bucket_id }
public fun amount<U, S, C>(rfq: &RfqAuction<U, S, C>): u64 { rfq.amount }
public fun reserve_premium<U, S, C>(rfq: &RfqAuction<U, S, C>): u64 { rfq.reserve_premium }
public fun deadline_ms<U, S, C>(rfq: &RfqAuction<U, S, C>): u64 { rfq.deadline_ms }
public fun max_deadline_ms<U, S, C>(rfq: &RfqAuction<U, S, C>): u64 { rfq.max_deadline_ms }
public fun origin<U, S, C>(rfq: &RfqAuction<U, S, C>): ID { rfq.origin }
public fun best_bidder<U, S, C>(rfq: &RfqAuction<U, S, C>): Option<address> { rfq.best_bidder }

/// The standing best premium (0 when no bids).
public fun best_premium<U, S, C>(rfq: &RfqAuction<U, S, C>): u64 {
    rfq.bid_escrow.value()
}

public fun position_recipient<U, S, C>(rfq: &RfqAuction<U, S, C>): address {
    rfq.position_recipient
}

public fun proceeds_recipient<U, S, C>(rfq: &RfqAuction<U, S, C>): address {
    rfq.proceeds_recipient
}
