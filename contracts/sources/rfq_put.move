/// On-chain RFQ for cash-secured puts — the mirror of `rfq.move`.
///
/// A seller escrows the put's CASH COLLATERAL (settlement) and opens an
/// ascending premium auction; MMs escrow premium bids (also settlement);
/// after the deadline anyone settles — the contract executes the
/// cash-secured write against the best bid. Both escrows are settlement
/// here (collateral and premium), versus `rfq.move` where the collateral
/// is underlying. Everything else — anti-snipe, reserve floor, escrowed
/// bids making the best bid always settleable, the coupled-venue path —
/// mirrors the call RFQ.
module options_protocol::rfq_put;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use options_protocol::admin::ProtocolConfig;
use options_protocol::bucket;
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position;
use options_protocol::put_bucket::{Self, PutBucket};
use options_protocol::treasury::Treasury;

const SETTLE_BUFFER_MS: u64 = 600_000; // 10 minutes
const MIN_DURATION_MS: u64 = 300_000; // 5 minutes
const BPS_DENOM: u128 = 10_000;

public struct PutRfqAuction<phantom Underlying, phantom Settlement, phantom Put> has key {
    id: UID,
    bucket_id: ID,
    /// Cash collateral escrowed for this slice; written into the bucket at
    /// settle. `== required_collateral(bucket, amount)`.
    collateral: Balance<Settlement>,
    /// Put notional in underlying units (NOT the collateral value).
    amount: u64,
    reserve_premium: u64,
    created_ms: u64,
    deadline_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_deadline_ms: u64,
    min_increment_bps: u64,
    best_bidder: Option<address>,
    best_put_recipient: Option<address>,
    bid_escrow: Balance<Settlement>,
    position_recipient: address,
    proceeds_recipient: address,
    origin: ID,
    coupled: bool,
}

/// The winning side of a finalized auction, handed back to a venue module.
public struct FinalizedPutBid<phantom Settlement> {
    bidder: address,
    put_recipient: address,
    premium: Balance<Settlement>,
}

public struct PutRfqReceipt has copy, drop {
    rfq_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    reserve_premium: u64,
}

/// Open an auction: escrow `collateral` cash for a put of `amount`
/// underlying-units and start the clock. Seller-agnostic — any holder of
/// settlement can auction a cash-secured write.
public fun create<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    collateral: Coin<Settlement>,
    amount: u64,
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
    create_impl(
        bucket,
        collateral.into_balance(),
        amount,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        position_recipient,
        proceeds_recipient,
        origin,
        false,
        clock,
        ctx,
    )
}

/// Venue-module variant: escrow arrives as a `Balance` and the auction is
/// coupled, so only the venue's own settle path can resolve it.
public(package) fun create_coupled<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    collateral: Balance<Settlement>,
    amount: u64,
    reserve_premium: u64,
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
    create_impl(
        bucket,
        collateral,
        amount,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        origin_addr,
        origin_addr,
        origin,
        true,
        clock,
        ctx,
    )
}

fun create_impl<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    collateral: Balance<Settlement>,
    amount: u64,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    position_recipient: address,
    proceeds_recipient: address,
    origin: ID,
    coupled: bool,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(amount > 0, errors::zero_amount());
    assert!(
        collateral.value() == put_bucket::required_collateral(bucket, amount),
        errors::put_collateral_mismatch(),
    );
    assert!(!put_bucket::invalidated(bucket), errors::bucket_invalidated());
    let now = clock.timestamp_ms();
    assert!(now < put_bucket::expiry_ms(bucket), errors::bucket_expired());
    assert!(duration_ms >= MIN_DURATION_MS, errors::rfq_duration_too_short());

    let deadline_ms = now + duration_ms;
    let max_deadline_ms = deadline_ms + max_extension_ms;
    assert!(
        max_deadline_ms + SETTLE_BUFFER_MS <= put_bucket::expiry_ms(bucket),
        errors::rfq_too_close_to_expiry(),
    );

    let collateral_amount = collateral.value();
    let rfq = PutRfqAuction<Underlying, Settlement, Put> {
        id: object::new(ctx),
        bucket_id: object::id(bucket),
        collateral,
        amount,
        reserve_premium,
        created_ms: now,
        deadline_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_deadline_ms,
        min_increment_bps,
        best_bidder: option::none(),
        best_put_recipient: option::none(),
        bid_escrow: balance::zero<Settlement>(),
        position_recipient,
        proceeds_recipient,
        origin,
        coupled,
    };
    let rfq_id = object::id(&rfq);
    events::emit_put_rfq_created(
        rfq_id,
        rfq.bucket_id,
        origin,
        amount,
        collateral_amount,
        reserve_premium,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps,
    );
    transfer::share_object(rfq);
    rfq_id
}

/// Escrow a premium bid. Must beat `max(reserve, best × (1 + increment))`
/// and strictly beat the standing best. The previous best is refunded by
/// push transfer.
public fun bid<Underlying, Settlement, Put>(
    rfq: &mut PutRfqAuction<Underlying, Settlement, Put>,
    premium_in: Coin<Settlement>,
    put_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < rfq.deadline_ms, errors::rfq_closed());

    let value = premium_in.value();
    let previous = rfq.bid_escrow.value();
    let floor = if (rfq.best_bidder.is_some()) {
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

    if (rfq.best_bidder.is_some()) {
        let refund = coin::from_balance(rfq.bid_escrow.withdraw_all(), ctx);
        transfer::public_transfer(refund, *rfq.best_bidder.borrow());
    };
    rfq.bid_escrow.join(premium_in.into_balance());
    rfq.best_bidder = option::some(ctx.sender());
    rfq.best_put_recipient = option::some(put_recipient);

    if (rfq.deadline_ms - now < rfq.snipe_window_ms) {
        let extended = now + rfq.snipe_extension_ms;
        rfq.deadline_ms = extended.min(rfq.max_deadline_ms);
    };

    events::emit_put_rfq_bid(
        object::id(rfq),
        ctx.sender(),
        put_recipient,
        value,
        previous,
        rfq.deadline_ms,
    );
}

/// Consume a closed auction; returns `(winner, collateral, receipt)`.
public(package) fun finalize<Underlying, Settlement, Put>(
    rfq: PutRfqAuction<Underlying, Settlement, Put>,
    clock: &Clock,
): (Option<FinalizedPutBid<Settlement>>, Balance<Settlement>, PutRfqReceipt) {
    assert!(clock.timestamp_ms() >= rfq.deadline_ms, errors::rfq_not_closed());
    destroy(rfq)
}

fun destroy<Underlying, Settlement, Put>(
    rfq: PutRfqAuction<Underlying, Settlement, Put>,
): (Option<FinalizedPutBid<Settlement>>, Balance<Settlement>, PutRfqReceipt) {
    let PutRfqAuction {
        id,
        bucket_id,
        collateral,
        amount,
        reserve_premium,
        created_ms: _,
        deadline_ms: _,
        snipe_window_ms: _,
        snipe_extension_ms: _,
        max_deadline_ms: _,
        min_increment_bps: _,
        mut best_bidder,
        mut best_put_recipient,
        bid_escrow,
        position_recipient: _,
        proceeds_recipient: _,
        origin,
        coupled: _,
    } = rfq;
    let receipt = PutRfqReceipt {
        rfq_id: id.to_inner(),
        bucket_id,
        origin,
        amount,
        reserve_premium,
    };
    id.delete();

    let winner = if (best_bidder.is_some()) {
        option::some(FinalizedPutBid {
            bidder: best_bidder.extract(),
            put_recipient: best_put_recipient.extract(),
            premium: bid_escrow,
        })
    } else {
        bid_escrow.destroy_zero();
        option::none()
    };
    best_bidder.destroy_none();
    best_put_recipient.destroy_none();
    (winner, collateral, receipt)
}

/// Settle a closed auction — callable by anyone. Winner: fee skimmed, the
/// cash-secured write executes, the winner gets `Coin<Put>`, the seller
/// gets the `Position` + net premium. No winner: collateral refunded.
public fun settle<Underlying, Settlement, Put>(
    rfq: PutRfqAuction<Underlying, Settlement, Put>,
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    assert!(!rfq.coupled, errors::vault_wrong_origin());
    let position_recipient = rfq.position_recipient;
    let proceeds_recipient = rfq.proceeds_recipient;
    let (mut winner, collateral, receipt) = finalize(rfq, clock);

    if (winner.is_some()) {
        let FinalizedPutBid { bidder, put_recipient, premium } = winner.extract();
        winner.destroy_none();
        let gross_premium = premium.value();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();

        let (pos, put) = put_bucket::write_collateralized_balance(
            bucket,
            collateral,
            receipt.amount,
            clock,
            ctx,
        );
        let position_id = object::id(&pos);
        let range_start = position::range_start(&pos);
        let range_end = position::range_end(&pos);
        transfer::public_transfer(put, put_recipient);
        transfer::public_transfer(pos, position_recipient);
        transfer::public_transfer(coin::from_balance(net, ctx), proceeds_recipient);

        events::emit_put_rfq_settled(
            receipt.rfq_id,
            receipt.bucket_id,
            receipt.origin,
            bidder,
            put_recipient,
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
        transfer::public_transfer(coin::from_balance(collateral, ctx), proceeds_recipient);
        events::emit_put_rfq_expired_unsold(
            receipt.rfq_id,
            receipt.bucket_id,
            receipt.origin,
            receipt.amount,
            receipt.reserve_premium,
        );
    };
}

/// Recovery path: the bucket expired or was invalidated mid-auction, so the
/// write can never execute — refund both escrows (bid → bidder, collateral →
/// proceeds recipient). Callable by anyone, no deadline precondition.
public(package) fun finalize_expired<Underlying, Settlement, Put>(
    rfq: PutRfqAuction<Underlying, Settlement, Put>,
    bucket: &PutBucket<Underlying, Settlement, Put>,
    clock: &Clock,
): (Option<FinalizedPutBid<Settlement>>, Balance<Settlement>, PutRfqReceipt) {
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    assert!(
        clock.timestamp_ms() >= put_bucket::expiry_ms(bucket) || put_bucket::invalidated(bucket),
        errors::bucket_not_expired(),
    );
    destroy(rfq)
}

public fun settle_expired<Underlying, Settlement, Put>(
    rfq: PutRfqAuction<Underlying, Settlement, Put>,
    bucket: &PutBucket<Underlying, Settlement, Put>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(!rfq.coupled, errors::vault_wrong_origin());
    let proceeds_recipient = rfq.proceeds_recipient;
    let (mut winner, collateral, receipt) = finalize_expired(rfq, bucket, clock);

    if (winner.is_some()) {
        let FinalizedPutBid { bidder, put_recipient: _, premium } = winner.extract();
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    transfer::public_transfer(coin::from_balance(collateral, ctx), proceeds_recipient);
    events::emit_put_rfq_expired_unsold(
        receipt.rfq_id,
        receipt.bucket_id,
        receipt.origin,
        receipt.amount,
        receipt.reserve_premium,
    );
}

// ---- FinalizedPutBid / PutRfqReceipt accessors (venue settle path) ----

public(package) fun bid_bidder<S>(b: &FinalizedPutBid<S>): address { b.bidder }
public(package) fun bid_put_recipient<S>(b: &FinalizedPutBid<S>): address { b.put_recipient }

public(package) fun unpack_bid<S>(b: FinalizedPutBid<S>): (address, address, Balance<S>) {
    let FinalizedPutBid { bidder, put_recipient, premium } = b;
    (bidder, put_recipient, premium)
}

public(package) fun receipt_rfq_id(r: &PutRfqReceipt): ID { r.rfq_id }
public(package) fun receipt_bucket_id(r: &PutRfqReceipt): ID { r.bucket_id }
public(package) fun receipt_origin(r: &PutRfqReceipt): ID { r.origin }
public(package) fun receipt_amount(r: &PutRfqReceipt): u64 { r.amount }
public(package) fun receipt_reserve_premium(r: &PutRfqReceipt): u64 { r.reserve_premium }

// ---- getters ----

public fun bucket_id<U, S, P>(rfq: &PutRfqAuction<U, S, P>): ID { rfq.bucket_id }
public fun amount<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 { rfq.amount }
public fun collateral<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 { rfq.collateral.value() }
public fun reserve_premium<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 { rfq.reserve_premium }
public fun deadline_ms<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 { rfq.deadline_ms }
public fun max_deadline_ms<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 { rfq.max_deadline_ms }
public fun origin<U, S, P>(rfq: &PutRfqAuction<U, S, P>): ID { rfq.origin }
public fun best_bidder<U, S, P>(rfq: &PutRfqAuction<U, S, P>): Option<address> { rfq.best_bidder }

public fun best_premium<U, S, P>(rfq: &PutRfqAuction<U, S, P>): u64 {
    rfq.bid_escrow.value()
}

public fun position_recipient<U, S, P>(rfq: &PutRfqAuction<U, S, P>): address {
    rfq.position_recipient
}

public fun proceeds_recipient<U, S, P>(rfq: &PutRfqAuction<U, S, P>): address {
    rfq.proceeds_recipient
}
