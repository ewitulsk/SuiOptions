/// Option-RFQ adapters over the generic `auction` package: the
/// options-specific ends (bucket validation at create, covered-write /
/// cash-secured-write settlement) of what used to be the monolithic
/// `rfq.move` and `rfq_put.move` venues. The auction mechanics — escrowed
/// bids, reserve floor, min-increment, anti-snipe, refunds — live in
/// `auction::auction`; bidders bid on the generic `Auction` object.
///
/// Each option RFQ is a pair of shared objects: the generic
/// `Auction<Escrow, Settlement>` (owned by the machine) and a small typed
/// metadata object here (`CallRfq` / `PutRfq`) binding it to its bucket
/// and payout routing. The adapter holds the auction's settle authority
/// (the `RfqAuth` witness), so settlement can only flow through the
/// settle functions below — which are themselves permissionless cranks.
module options_rfq::rfq;

use sui::balance::Balance;
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use auction::auction::{Self as auctions, Auction, FinalizedBid};
use options_core::admin::ProtocolConfig;
use options_core::bucket::{Self, Bucket};
use options_core::errors as core_errors;
use options_core::position;
use options_core::put_bucket::{Self, PutBucket};
use options_core::treasury::Treasury;

use options_rfq::errors;
use options_rfq::events;

/// Guarantees the settle crank can land while the bucket still accepts
/// writes: the auction's hard deadline must precede bucket expiry by at
/// least this much.
const SETTLE_BUFFER_MS: u64 = 600_000; // 10 minutes

/// Settle authority for adapter-created auctions. Only this module can
/// mint it, so only the settle paths below can resolve those auctions.
public struct RfqAuth has drop {}

/// Metadata binding a generic auction to the covered-call write it will
/// settle into. `amount` == the escrowed underlying.
public struct CallRfq<phantom Underlying, phantom Settlement, phantom Call> has key {
    id: UID,
    auction_id: ID,
    bucket_id: ID,
    /// Caller-supplied attribution (seller address-as-ID).
    origin: ID,
    amount: u64,
    reserve_premium: u64,
    position_recipient: address,
    proceeds_recipient: address,
}

/// Metadata binding a generic auction to the cash-secured-put write it
/// will settle into. `amount` is the option notional in underlying units;
/// the escrow is `required_collateral(bucket, amount)` of settlement.
public struct PutRfq<phantom Underlying, phantom Settlement, phantom Put> has key {
    id: UID,
    auction_id: ID,
    bucket_id: ID,
    origin: ID,
    amount: u64,
    collateral: u64,
    reserve_premium: u64,
    position_recipient: address,
    proceeds_recipient: address,
}

/// Open a covered-call RFQ: validate the bucket, escrow the underlying
/// into a coupled generic auction, and share the metadata binding.
/// Deliberately public and seller-agnostic — any holder of underlying can
/// auction a covered write. Returns `(rfq_id, auction_id)`.
public fun create_call_auction<Underlying, Settlement, Call>(
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
): (ID, ID) {
    let amount = underlying.value();
    assert_bucket_open(
        bucket::invalidated(bucket),
        bucket::expiry_ms(bucket),
        duration_ms,
        max_extension_ms,
        clock,
    );

    let mut meta = CallRfq<Underlying, Settlement, Call> {
        id: object::new(ctx),
        auction_id: origin, // placeholder, assigned below
        bucket_id: object::id(bucket),
        origin,
        amount,
        reserve_premium,
        position_recipient,
        proceeds_recipient,
    };
    let rfq_id = object::id(&meta);
    let auction_id = auctions::create_coupled<Underlying, Settlement, RfqAuth>(
        RfqAuth {},
        underlying.into_balance(),
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        rfq_id,
        clock,
        ctx,
    );
    meta.auction_id = auction_id;
    events::emit_rfq_created(
        rfq_id,
        auction_id,
        meta.bucket_id,
        origin,
        amount,
        reserve_premium,
    );
    transfer::share_object(meta);
    (rfq_id, auction_id)
}

/// Open a cash-secured-put RFQ: the escrow is the exact cash collateral
/// for `amount` notional; bids are premium in the same settlement asset.
public fun create_put_auction<Underlying, Settlement, Put>(
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
): (ID, ID) {
    assert!(
        collateral.value() == put_bucket::required_collateral(bucket, amount),
        core_errors::put_collateral_mismatch(),
    );
    assert_bucket_open(
        put_bucket::invalidated(bucket),
        put_bucket::expiry_ms(bucket),
        duration_ms,
        max_extension_ms,
        clock,
    );

    let collateral_amount = collateral.value();
    let mut meta = PutRfq<Underlying, Settlement, Put> {
        id: object::new(ctx),
        auction_id: origin, // placeholder, assigned below
        bucket_id: object::id(bucket),
        origin,
        amount,
        collateral: collateral_amount,
        reserve_premium,
        position_recipient,
        proceeds_recipient,
    };
    let rfq_id = object::id(&meta);
    let auction_id = auctions::create_coupled<Settlement, Settlement, RfqAuth>(
        RfqAuth {},
        collateral.into_balance(),
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        rfq_id,
        clock,
        ctx,
    );
    meta.auction_id = auction_id;
    events::emit_put_rfq_created(
        rfq_id,
        auction_id,
        meta.bucket_id,
        origin,
        amount,
        collateral_amount,
        reserve_premium,
    );
    transfer::share_object(meta);
    (rfq_id, auction_id)
}

/// Bucket preconditions shared by both create paths: alive, unexpired,
/// and the auction's hard deadline leaves the settle crank room before
/// expiry.
fun assert_bucket_open(
    invalidated: bool,
    expiry_ms: u64,
    duration_ms: u64,
    max_extension_ms: u64,
    clock: &Clock,
) {
    assert!(!invalidated, core_errors::bucket_invalidated());
    let now = clock.timestamp_ms();
    assert!(now < expiry_ms, core_errors::bucket_expired());
    assert!(
        now + duration_ms + max_extension_ms + SETTLE_BUFFER_MS <= expiry_ms,
        errors::rfq_too_close_to_expiry(),
    );
}

/// Settle a closed covered-call RFQ — callable by anyone. Winner: the
/// protocol fee is skimmed, the covered write executes, the winner's
/// token recipient gets `Coin<Call>`, the seller gets the `Position` +
/// net premium. No winner: the underlying is refunded to the proceeds
/// recipient.
public fun settle_call<Underlying, Settlement, Call>(
    rfq: CallRfq<Underlying, Settlement, Call>,
    auction: Auction<Underlying, Settlement>,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let (meta, winner, underlying) = finalize_call(rfq, auction, bucket, clock, false);
    settle_call_outputs(meta, winner, underlying, bucket, config, treasury, clock, ctx);
}

/// Recovery path: the bucket expired or was invalidated mid-auction, so
/// the write can never execute — refund both escrows (bid → bidder,
/// underlying → proceeds recipient) so funds can never strand. Callable
/// by anyone, with no deadline precondition: once the bucket is dead the
/// auction is moot.
public fun settle_call_expired<Underlying, Settlement, Call>(
    rfq: CallRfq<Underlying, Settlement, Call>,
    auction: Auction<Underlying, Settlement>,
    bucket: &Bucket<Underlying, Settlement, Call>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        clock.timestamp_ms() >= bucket::expiry_ms(bucket) || bucket::invalidated(bucket),
        core_errors::bucket_not_expired(),
    );
    let (meta, mut winner, underlying) = finalize_call(rfq, auction, bucket, clock, true);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    transfer::public_transfer(
        coin::from_balance(underlying, ctx),
        meta.proceeds_recipient,
    );
    emit_call_unsold(&meta);
    destroy_call_meta(meta);
}

fun finalize_call<Underlying, Settlement, Call>(
    rfq: CallRfq<Underlying, Settlement, Call>,
    auction: Auction<Underlying, Settlement>,
    bucket: &Bucket<Underlying, Settlement, Call>,
    clock: &Clock,
    early: bool,
): (
    CallRfq<Underlying, Settlement, Call>,
    Option<FinalizedBid<Settlement>>,
    Balance<Underlying>,
) {
    assert!(rfq.auction_id == object::id(&auction), errors::rfq_auction_mismatch());
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    let (winner, underlying, _receipt) = if (early) {
        auctions::finalize_early<Underlying, Settlement, RfqAuth>(RfqAuth {}, auction)
    } else {
        auctions::finalize<Underlying, Settlement, RfqAuth>(RfqAuth {}, auction, clock)
    };
    (rfq, winner, underlying)
}

fun settle_call_outputs<Underlying, Settlement, Call>(
    meta: CallRfq<Underlying, Settlement, Call>,
    mut winner: Option<FinalizedBid<Settlement>>,
    underlying: Balance<Underlying>,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    if (winner.is_some()) {
        let (bidder, call_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let gross_premium = premium.value();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();

        let (pos, call) = bucket::write_collateralized_balance(bucket, underlying, clock, ctx);
        let position_id = object::id(&pos);
        let range_start = position::range_start(&pos);
        let range_end = position::range_end(&pos);
        transfer::public_transfer(call, call_recipient);
        transfer::public_transfer(pos, meta.position_recipient);
        transfer::public_transfer(coin::from_balance(net, ctx), meta.proceeds_recipient);

        events::emit_rfq_settled(
            object::id(&meta),
            meta.auction_id,
            meta.bucket_id,
            meta.origin,
            bidder,
            call_recipient,
            position_id,
            meta.position_recipient,
            meta.amount,
            gross_premium,
            fee,
            net_premium,
            range_start,
            range_end,
        );
    } else {
        winner.destroy_none();
        transfer::public_transfer(
            coin::from_balance(underlying, ctx),
            meta.proceeds_recipient,
        );
        emit_call_unsold(&meta);
    };
    destroy_call_meta(meta);
}

fun emit_call_unsold<U, S, C>(meta: &CallRfq<U, S, C>) {
    events::emit_rfq_expired_unsold(
        object::id(meta),
        meta.auction_id,
        meta.bucket_id,
        meta.origin,
        meta.amount,
        meta.reserve_premium,
    );
}

fun destroy_call_meta<U, S, C>(meta: CallRfq<U, S, C>) {
    let CallRfq { id, .. } = meta;
    id.delete();
}

/// Settle a closed cash-secured-put RFQ — callable by anyone. Winner:
/// fee skimmed, the collateralized put write executes, the winner's
/// token recipient gets `Coin<Put>`, the seller gets the `Position` +
/// net premium. No winner: the collateral is refunded to the proceeds
/// recipient.
public fun settle_put<Underlying, Settlement, Put>(
    rfq: PutRfq<Underlying, Settlement, Put>,
    auction: Auction<Settlement, Settlement>,
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(rfq.auction_id == object::id(&auction), errors::rfq_auction_mismatch());
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    let (mut winner, collateral, _receipt) =
        auctions::finalize<Settlement, Settlement, RfqAuth>(RfqAuth {}, auction, clock);

    if (winner.is_some()) {
        let (bidder, put_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let gross_premium = premium.value();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();

        let (pos, put) = put_bucket::write_collateralized_balance(
            bucket,
            collateral,
            rfq.amount,
            clock,
            ctx,
        );
        let position_id = object::id(&pos);
        let range_start = position::range_start(&pos);
        let range_end = position::range_end(&pos);
        transfer::public_transfer(put, put_recipient);
        transfer::public_transfer(pos, rfq.position_recipient);
        transfer::public_transfer(coin::from_balance(net, ctx), rfq.proceeds_recipient);

        events::emit_put_rfq_settled(
            object::id(&rfq),
            rfq.auction_id,
            rfq.bucket_id,
            rfq.origin,
            bidder,
            put_recipient,
            position_id,
            rfq.position_recipient,
            rfq.amount,
            gross_premium,
            fee,
            net_premium,
            range_start,
            range_end,
        );
    } else {
        winner.destroy_none();
        transfer::public_transfer(
            coin::from_balance(collateral, ctx),
            rfq.proceeds_recipient,
        );
        emit_put_unsold(&rfq);
    };
    destroy_put_meta(rfq);
}

/// Put twin of `settle_call_expired`: bucket died mid-auction — refund
/// both escrows.
public fun settle_put_expired<Underlying, Settlement, Put>(
    rfq: PutRfq<Underlying, Settlement, Put>,
    auction: Auction<Settlement, Settlement>,
    bucket: &PutBucket<Underlying, Settlement, Put>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(rfq.auction_id == object::id(&auction), errors::rfq_auction_mismatch());
    assert!(rfq.bucket_id == object::id(bucket), errors::rfq_bucket_mismatch());
    assert!(
        clock.timestamp_ms() >= put_bucket::expiry_ms(bucket)
            || put_bucket::invalidated(bucket),
        core_errors::bucket_not_expired(),
    );
    let (mut winner, collateral, _receipt) =
        auctions::finalize_early<Settlement, Settlement, RfqAuth>(RfqAuth {}, auction);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    transfer::public_transfer(
        coin::from_balance(collateral, ctx),
        rfq.proceeds_recipient,
    );
    emit_put_unsold(&rfq);
    destroy_put_meta(rfq);
}

fun emit_put_unsold<U, S, P>(meta: &PutRfq<U, S, P>) {
    events::emit_put_rfq_expired_unsold(
        object::id(meta),
        meta.auction_id,
        meta.bucket_id,
        meta.origin,
        meta.amount,
        meta.reserve_premium,
    );
}

fun destroy_put_meta<U, S, P>(meta: PutRfq<U, S, P>) {
    let PutRfq { id, .. } = meta;
    id.delete();
}

// ---- getters (off-chain reads) ----

public fun call_auction_id<U, S, C>(rfq: &CallRfq<U, S, C>): ID { rfq.auction_id }
public fun call_bucket_id<U, S, C>(rfq: &CallRfq<U, S, C>): ID { rfq.bucket_id }
public fun call_origin<U, S, C>(rfq: &CallRfq<U, S, C>): ID { rfq.origin }
public fun call_amount<U, S, C>(rfq: &CallRfq<U, S, C>): u64 { rfq.amount }
public fun call_reserve_premium<U, S, C>(rfq: &CallRfq<U, S, C>): u64 { rfq.reserve_premium }
public fun call_position_recipient<U, S, C>(rfq: &CallRfq<U, S, C>): address {
    rfq.position_recipient
}
public fun call_proceeds_recipient<U, S, C>(rfq: &CallRfq<U, S, C>): address {
    rfq.proceeds_recipient
}

public fun put_auction_id<U, S, P>(rfq: &PutRfq<U, S, P>): ID { rfq.auction_id }
public fun put_bucket_id<U, S, P>(rfq: &PutRfq<U, S, P>): ID { rfq.bucket_id }
public fun put_origin<U, S, P>(rfq: &PutRfq<U, S, P>): ID { rfq.origin }
public fun put_amount<U, S, P>(rfq: &PutRfq<U, S, P>): u64 { rfq.amount }
public fun put_collateral<U, S, P>(rfq: &PutRfq<U, S, P>): u64 { rfq.collateral }
public fun put_reserve_premium<U, S, P>(rfq: &PutRfq<U, S, P>): u64 { rfq.reserve_premium }
public fun put_position_recipient<U, S, P>(rfq: &PutRfq<U, S, P>): address {
    rfq.position_recipient
}
public fun put_proceeds_recipient<U, S, P>(rfq: &PutRfq<U, S, P>): address {
    rfq.proceeds_recipient
}
