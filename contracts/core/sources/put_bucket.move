/// Cash-secured puts — the structural mirror of `bucket.move`.
///
/// A cash-secured put is the same pooled-bucket + monotonic-cursor design as
/// the covered call, with the asset legs flipped:
///
///   covered call                cash-secured put
///   ─────────────────────────   ────────────────────────────────────────
///   collateral = underlying     collateral = SETTLEMENT (cash)
///   exercise: pay cash, take U  exercise: deliver U, take cash
///   redeem exercised  → cash    redeem exercised  → underlying
///   redeem unexercised→ U       redeem unexercised→ cash
///
/// The FIFO cursor, the `Position` object, and the `Quote` are identical and
/// reused unchanged. `total_written`/`exercise_cursor`/the position ranges
/// are all denominated in UNDERLYING smallest-units, exactly as for calls —
/// `write_amount` is "how many underlying units this put covers". The cash
/// collateral for that write is `write_amount × strike`.
///
/// The bucket owns the sole `TreasuryCap<Put>` for its per-bucket put coin,
/// so (as with calls) outstanding supply == outstanding options and bucket
/// isolation is a type-system guarantee, not a runtime check.
///
/// ─────────────────────────────────────────────────────────────────────────
/// PRICING / ROUNDING (the cash leg)
/// ─────────────────────────────────────────────────────────────────────────
/// Strike is a scaled ratio: real cash-per-underlying-unit = `strike /
/// 10^strike_scale` (same representation as `bucket.move`). The cash leg
/// therefore rounds, and the rounding direction is chosen to make the bucket
/// **provably solvent** — it can never owe more cash than it holds:
///
///   • Collateral IN (at write) rounds UP   — `apply_strike_ceil`.
///     The writer posts `ceil(write_amount × strike)`, i.e. at least the full
///     worst-case obligation for every unit they wrote.
///   • Cash OUT (exercise payout to the holder, and unexercised-collateral
///     returned to the writer at redeem) rounds DOWN — `apply_strike_floor`.
///
/// Solvency proof (settlement balance `B`, totals over the bucket's life):
///   after all writes:     B = Σ ceil(wᵢ·s)            ≥ Σ wᵢ·s = W·s
///   exercises subtract:   Σ floor(nⱼ·s)               ≤ E·s        (E ≤ W)
///   ⇒ after exercises:    B ≥ W·s − E·s = (W−E)·s = U·s
///   redeems subtract:     Σ floor(uᵢ·s)               ≤ U·s ≤ B
/// (s = strike/10^scale, W = total_written, E = total exercised, U = W−E.)
/// Exercises are pre-expiry and redeems post-expiry, so every exercise
/// strictly precedes every redeem and `B` never goes negative. The underlying
/// leg is exact integer accounting (units delivered in == units handed back),
/// so it always drains to zero.
///
/// The price of guaranteed solvency is **dust**: a tiny non-negative cash
/// remainder `Σ ceil(wᵢ·s) − Σ floor(nⱼ·s) − Σ floor(uᵢ·s)` can be left in
/// the bucket after everyone has claimed. It is bounded by ~1 settlement
/// smallest-unit per write/exercise/redeem (sub-cent for USDC) and is swept
/// to the admin at `cleanup_bucket`. A writer can likewise forgo up to ~1
/// smallest-unit of their own returned collateral to rounding; this is the
/// holder-favoring direction and matches the spirit of the call side's
/// round-half-up dust.
module options_core::put_bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};

use sui::dynamic_field as df;

use options_core::admin::{AdminCap, ProtocolConfig};
use whitelist::whitelist::{Self, Whitelist};
use options_core::bucket;
use options_core::collateral::{Self, CollateralRequest};
use options_core::errors;
use options_core::events;
use options_core::position::{Self, Position};
use options_core::quote::{Self, Quote, SignedQuote};
use options_core::quote_signer::QuoteSigner;
use options_core::treasury::Treasury;

/// Mirror of `bucket::MAX_STRIKE_SCALE` (10^38 is the largest power of ten
/// that fits in u128). The shared `bucket::pow10` enforces the same cap.
const MAX_STRIKE_SCALE: u8 = 38;

public struct PutBucket<phantom Underlying, phantom Settlement, phantom Put> has key {
    id: UID,
    asset_type: TypeName,
    settlement_type: TypeName,
    put_type: TypeName,
    expiry_ms: u64,
    /// Real ratio = `strike / 10^strike_scale` (cash per underlying-unit).
    strike: u128,
    strike_scale: u8,
    /// In underlying smallest-units (the put's notional), same as calls.
    total_written: u128,
    exercise_cursor: u128,
    /// Sum of redeemed position ranges; `== total_written` once every
    /// position has been redeemed, which is the cleanup precondition (the
    /// cash leg leaves dust, so a `settlement_balance == 0` gate — as the
    /// call bucket uses — would be unreachable for fractional strikes).
    total_redeemed: u128,
    /// Underlying delivered by exercisers, claimed by assigned writers.
    underlying_balance: Balance<Underlying>,
    /// The cash collateral (plus rounding dust).
    settlement_balance: Balance<Settlement>,
    /// Sole mint/burn authority for the put coin. Never exposed by ref.
    put_treasury: TreasuryCap<Put>,
    invalidated: bool,
    /// Exact-offset closure tombstones — the put mirror of
    /// `bucket::Bucket.closed`: sorted, disjoint, every interval ≥ the
    /// cursor; the cursor skips them. See `close_offset`.
    closed: vector<PutClosedInterval>,
    /// Total units across `closed` — subtracted from exercise capacity.
    closed_pending: u128,
    /// Live compressed (spread-backed) ranges, sorted by start (FIFO
    /// append order). Unlike the call side, the cursor CAN pass through
    /// one — but only via `exercise_spread` (assignment funds the long
    /// exercise); the plain path refuses. A range stays here, possibly
    /// partially assigned, until `close_spread` / `redeem_spread_position`
    /// retires it. See the "put spread compression" section.
    spreads: vector<PutSpreadRange>,
}

/// A tombstoned (offset-closed) slice of the write space.
public struct PutClosedInterval has copy, drop, store { start: u128, end: u128 }

/// One live compressed range.
public struct PutSpreadRange has copy, drop, store { start: u128, end: u128 }

/// Dynamic-field key for a compressed range's escrow, by range start.
public struct PutSpreadEscrowKey has copy, drop, store { start: u128 }

/// What backs a compressed put range: the long puts (consumed pro-rata
/// as the range is assigned) plus the gap cash — sized so escrow + long
/// proceeds provably cover every assignment path (see `write_spread`).
public struct PutSpreadEscrow<phantom LongPut, phantom Settlement> has store {
    long: Balance<LongPut>,
    cash: Balance<Settlement>,
    long_bucket_id: ID,
}

/// ceil((amount × strike) / 10^strike_scale) — collateral sizing.
fun apply_strike_ceil(amount: u128, strike: u128, strike_scale: u8): u64 {
    let divisor = bucket::pow10(strike_scale);
    let numerator = amount * strike;
    ((numerator + divisor - 1) / divisor) as u64
}

/// floor((amount × strike) / 10^strike_scale) — every cash payout.
fun apply_strike_floor(amount: u128, strike: u128, strike_scale: u8): u64 {
    let divisor = bucket::pow10(strike_scale);
    ((amount * strike) / divisor) as u64
}

/// Cash collateral required to write `amount` underlying-units of this put.
public fun required_collateral<U, S, P>(bucket: &PutBucket<U, S, P>, amount: u64): u64 {
    apply_strike_ceil(amount as u128, bucket.strike, bucket.strike_scale)
}

/// Settlement paid out for exercising `amount` put units (floor rounding —
/// matches `exercise`).
public fun exercise_payout<U, S, P>(bucket: &PutBucket<U, S, P>, amount: u64): u64 {
    apply_strike_floor(amount as u128, bucket.strike, bucket.strike_scale)
}

/// Create one put bucket for the (Underlying, Settlement, Put) triple, taking
/// ownership of the put coin's fresh (zero-supply) `TreasuryCap`. Mirrors
/// `bucket::create_bucket`; the off-chain scheduler harvests the cap from a
/// per-roll OTW coin package exactly as it does for calls.
public fun create_put_bucket<Underlying, Settlement, Put>(
    _: &AdminCap,
    put_treasury: TreasuryCap<Put>,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
    ctx: &mut TxContext,
) {
    assert!(strike_scale <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    assert!(coin::total_supply(&put_treasury) == 0, errors::treasury_cap_not_fresh());

    let asset_type = type_name::with_defining_ids<Underlying>();
    let settlement_type = type_name::with_defining_ids<Settlement>();
    let put_type = type_name::with_defining_ids<Put>();
    let bucket = PutBucket<Underlying, Settlement, Put> {
        id: object::new(ctx),
        asset_type,
        settlement_type,
        put_type,
        expiry_ms,
        strike,
        strike_scale,
        total_written: 0,
        exercise_cursor: 0,
        total_redeemed: 0,
        underlying_balance: balance::zero<Underlying>(),
        settlement_balance: balance::zero<Settlement>(),
        put_treasury,
        invalidated: false,
        closed: vector[],
        closed_pending: 0,
        spreads: vector[],
    };
    let bucket_id = object::id(&bucket);
    events::emit_put_bucket_created(
        bucket_id,
        asset_type,
        settlement_type,
        put_type,
        expiry_ms,
        strike,
        strike_scale,
    );
    transfer::share_object(bucket);
}

/// Writer flow, step 1 (see `bucket::request_writer_flow` and
/// `collateral.move`): the signer is the trader MM (the put BUYER).
/// Demands the MM's PREMIUM as a `CollateralRequest<Settlement>`.
public fun request_writer_flow<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_and_consume_quote(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    collateral::new_writer_request<Settlement>(q, quote::premium(&q))
}

/// Trader flow, step 1: the signer is the writer MM (the put SELLER).
/// Demands the MM's cash write collateral —
/// `ceil(write_amount × strike)` — as a `CollateralRequest<Settlement>`.
/// Both put flows demand the settlement asset; the flow tag on the potato
/// keeps a premium-sized writer request out of `execute_trader_flow`.
public fun request_trader_flow<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_and_consume_quote(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    collateral::new_trader_request<Settlement>(
        q,
        required_collateral(bucket, quote::write_amount(&q)),
    )
}

fun assert_quote_bucket<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    q: &Quote,
    clock: &Clock,
) {
    assert!(quote::bucket_id(q) == object::id(bucket), errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    assert!(quote::write_amount(q) > 0, errors::zero_amount());
}

/// Writer flow, step 2: the executor (the retail put writer, tx sender)
/// posts the exact cash collateral and receives the `Position` + net
/// premium; the MM's `Coin<Put>` goes to the quote's
/// `signer_token_recipient`.
#[allow(lint(self_transfer))]
public fun execute_writer_flow<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    wl: &Whitelist,
    treasury: &mut Treasury,
    request: CollateralRequest<Settlement>,
    premium_funds: Balance<Settlement>,
    collateral_in: Coin<Settlement>,
    position_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    let (q, amount, is_writer) = collateral::destroy(request);
    assert!(is_writer, errors::request_flow_mismatch());
    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(premium_funds.value() == amount, errors::amount_mismatch());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let put_token_recipient = quote::signer_token_recipient(&q);
    let collateral_required = required_collateral(bucket, write_amount);
    assert!(collateral_in.value() == collateral_required, errors::put_collateral_mismatch());

    let (net_balance, fee) = bucket::skim_fee(config, treasury, premium_funds);
    let net_premium = gross_premium - fee;
    transfer::public_transfer(coin::from_balance(net_balance, ctx), ctx.sender());

    let (position, put) =
        write_and_check(bucket, collateral_in.into_balance(), write_amount, clock, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(put, put_token_recipient);

    events::emit_put_write_executed(
        bucket_id,
        quote::signer_id(&q),
        quote::collateral_source(&q),
        put_token_recipient,
        ctx.sender(),
        position_id,
        position_recipient,
        put_token_recipient,
        write_amount,
        collateral_required,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );
}

/// Trader flow, step 2: the executor (the retail put buyer, tx sender)
/// pays the premium and chooses where the `Coin<Put>` goes; the MM
/// receives the `Position` + net premium at `signer_token_recipient`.
public fun execute_trader_flow<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    wl: &Whitelist,
    treasury: &mut Treasury,
    request: CollateralRequest<Settlement>,
    collateral_funds: Balance<Settlement>,
    premium_in: Coin<Settlement>,
    put_token_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    let (q, amount, is_writer) = collateral::destroy(request);
    assert!(!is_writer, errors::request_flow_mismatch());
    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(collateral_funds.value() == amount, errors::amount_mismatch());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let position_recipient = quote::signer_token_recipient(&q);
    let collateral_required = required_collateral(bucket, write_amount);
    assert!(collateral_funds.value() == collateral_required, errors::put_collateral_mismatch());
    assert!(premium_in.value() == gross_premium, errors::amount_mismatch());

    let (net_balance, fee) = bucket::skim_fee(config, treasury, premium_in.into_balance());
    let net_premium = gross_premium - fee;
    transfer::public_transfer(coin::from_balance(net_balance, ctx), position_recipient);

    let (position, put) = write_and_check(bucket, collateral_funds, write_amount, clock, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(put, put_token_recipient);

    events::emit_put_write_executed(
        bucket_id,
        quote::signer_id(&q),
        quote::collateral_source(&q),
        position_recipient,
        ctx.sender(),
        position_id,
        position_recipient,
        put_token_recipient,
        write_amount,
        collateral_required,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );
}

fun write_and_check<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    collateral: Balance<Settlement>,
    write_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    do_write(bucket, collateral, write_amount, ctx)
}

/// Test-only request twins that skip signature verification.
#[test_only]
public fun request_writer_flow_for_testing<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_skip_sig(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    collateral::new_writer_request<Settlement>(q, quote::premium(&q))
}

#[test_only]
public fun request_trader_flow_for_testing<Underlying, Settlement, Put>(
    bucket: &PutBucket<Underlying, Settlement, Put>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_skip_sig(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    collateral::new_trader_request<Settlement>(
        q,
        required_collateral(bucket, quote::write_amount(&q)),
    )
}

/// Core cash-secured write: escrow `collateral_in` cash in the bucket and
/// mint the `Position` + `Coin<Put>`, returned to the caller (no transfers).
/// Mirrors `bucket::write_collateralized`; safe to expose `public` for the
/// same reason — the caller fully collateralizes every unit minted and holds
/// both sides until they part with the put coin.
public fun write_collateralized<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    wl: &Whitelist,
    collateral_in: Coin<Settlement>,
    write_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    write_collateralized_balance(
        bucket,
        wl,
        collateral_in.into_balance(),
        write_amount,
        clock,
        ctx,
    )
}

/// `Balance`-accepting sibling for venues (the put RFQ) whose escrow lives
/// as a `Balance`. Public and permissionless-safe: the exact cash
/// collateral is required in, `Position` + put coin out — no premium leg.
public fun write_collateralized_balance<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    wl: &Whitelist,
    collateral: Balance<Settlement>,
    write_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    assert!(write_amount > 0, errors::zero_amount());
    assert!(
        collateral.value() == required_collateral(bucket, write_amount),
        errors::put_collateral_mismatch(),
    );
    let collateral_amount = collateral.value();
    let (position, put) = do_write(bucket, collateral, write_amount, ctx);
    events::emit_put_collateralized_write(
        object::id(bucket),
        ctx.sender(),
        write_amount,
        collateral_amount,
        position::range_start(&position),
        position::range_end(&position),
    );
    (position, put)
}

/// Bucket mechanics shared by every put write venue: escrow the cash
/// collateral, advance the write cursor by `write_amount` (underlying
/// units — NOT the collateral value), mint the `Position` + `Coin<Put>`.
public(package) fun do_write<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    collateral: Balance<Settlement>,
    write_amount: u64,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    bucket.settlement_balance.join(collateral);

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(object::id(bucket), range_start, range_end, ctx);
    let put = coin::mint(&mut bucket.put_treasury, write_amount, ctx);
    (position, put)
}

/// Exercise `Coin<Put>` by DELIVERING the matching underlying and receiving
/// `floor(amount × strike)` cash out. The mirror of `bucket::exercise`.
public fun exercise<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    put: Coin<Put>,
    underlying_delivery: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Settlement> {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let bucket_id = object::id(bucket);

    let amount = put.value();
    assert!(amount > 0, errors::zero_amount());
    // The holder must deliver exactly one underlying unit per put unit.
    assert!(underlying_delivery.value() == amount, errors::amount_mismatch());
    assert!(
        bucket.exercise_cursor + (amount as u128) + bucket.closed_pending
            <= bucket.total_written,
        errors::cursor_overflow(),
    );

    // Burning through the bucket's own treasury enforces, by type, that the
    // coin belongs to this bucket.
    coin::burn(&mut bucket.put_treasury, put);

    bucket.underlying_balance.join(underlying_delivery.into_balance());
    advance_cursor(bucket, amount);

    let payout = apply_strike_floor(amount as u128, bucket.strike, bucket.strike_scale);
    let settlement = coin::from_balance(bucket.settlement_balance.split(payout), ctx);

    events::emit_put_exercised(
        bucket_id,
        ctx.sender(),
        amount,
        payout,
        bucket.exercise_cursor,
    );

    settlement
}

/// Redeem a put `Position` after expiry. Assigned (exercised) range returns
/// the DELIVERED UNDERLYING; the unassigned range returns the writer's cash
/// collateral (`floor(unexercised × strike)`).
public fun redeem_position<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    position: Position,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Underlying>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());

    let bucket_id = object::id(bucket);
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == bucket_id, errors::position_bucket_mismatch());
    // Spread positions settle from their escrow, never from the pool.
    assert!(!overlaps_spread(bucket, rs, re), errors::spread_position());

    let cursor = bucket.exercise_cursor;
    let exercised: u128 = if (cursor <= rs) {
        0
    } else if (cursor >= re) {
        re - rs
    } else {
        cursor - rs
    };
    let total_range = re - rs;
    let unexercised = total_range - exercised;
    bucket.total_redeemed = bucket.total_redeemed + total_range;

    // Exercised range → the underlying that holders delivered.
    let underlying_amount = exercised as u64;
    // Unexercised range → the writer's untouched cash collateral.
    let settlement_amount = apply_strike_floor(unexercised, bucket.strike, bucket.strike_scale);

    let underlying = coin::from_balance(
        bucket.underlying_balance.split(underlying_amount),
        ctx,
    );
    let settlement = coin::from_balance(
        bucket.settlement_balance.split(settlement_amount),
        ctx,
    );

    events::emit_put_redeemed(
        bucket_id,
        position_id,
        ctx.sender(),
        rs,
        re,
        underlying_amount,
        settlement_amount,
    );

    (underlying, settlement)
}

// ─────────────────────── exact-offset closure ───────────────────────

/// Net `put.value()` units of same-bucket put coins against the caller's
/// own `Position`, returning the freed cash collateral
/// (`floor(amount × strike)` — the same holder-favoring rounding as every
/// other cash payout; the ceil-floor delta stays as bucket dust). The put
/// mirror of `bucket::close_offset`: burn the coins, shrink the position
/// from its range end, tombstone the slice so the cursor skips it. The
/// closed units also count toward `total_redeemed` (they can never be
/// redeemed), keeping the cleanup gate reachable.
public fun close_offset<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    position: &mut Position,
    put: Coin<Put>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Settlement> {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let amount = put.value();
    assert!(amount > 0, errors::zero_amount());
    assert!(
        position::bucket_id(position) == object::id(bucket),
        errors::position_bucket_mismatch(),
    );
    let rs = position::range_start(position);
    let re = position::range_end(position);
    assert!((amount as u128) <= re - rs, errors::close_exceeds_position());
    let cut = re - (amount as u128);
    assert!(bucket.exercise_cursor <= cut, errors::close_range_exercised());
    // Spread positions are escrow-backed; they exit via `close_spread`.
    assert!(!overlaps_spread(bucket, cut, re), errors::spread_position());

    coin::burn(&mut bucket.put_treasury, put);
    position::shrink_end(position, amount as u128);
    insert_closed(bucket, cut, re);
    bucket.total_redeemed = bucket.total_redeemed + (amount as u128);
    let refund = apply_strike_floor(amount as u128, bucket.strike, bucket.strike_scale);
    let out = coin::from_balance(bucket.settlement_balance.split(refund), ctx);
    events::emit_offset_closed(
        object::id(bucket),
        ctx.sender(),
        object::id(position),
        true,
        amount,
        refund,
        cut,
        re,
    );
    out
}

/// Advance the cursor by `amount` exercisable units, jumping over closed
/// intervals (consuming them), refusing to enter a spread range — its
/// collateral is escrow, not pool cash, so only `exercise_spread` may
/// carry the cursor through it. Capacity was already checked against
/// `closed_pending`. Mirror of `bucket::advance_cursor`.
fun advance_cursor<U, S, P>(bucket: &mut PutBucket<U, S, P>, amount: u64) {
    let mut remaining = amount as u128;
    let mut cur = bucket.exercise_cursor;
    while (remaining > 0) {
        if (!bucket.closed.is_empty() && bucket.closed[0].start == cur) {
            let PutClosedInterval { start, end } = bucket.closed.remove(0);
            bucket.closed_pending = bucket.closed_pending - (end - start);
            cur = end;
            continue
        };
        let next_closed = if (bucket.closed.is_empty()) { bucket.total_written }
        else { bucket.closed[0].start };
        let next_spread = next_spread_start(bucket, cur);
        assert!(cur < next_spread, errors::put_spread_exercise_required());
        let limit = if (next_closed < next_spread) { next_closed } else { next_spread };
        let step = if (remaining < limit - cur) { remaining } else { limit - cur };
        cur = cur + step;
        remaining = remaining - step;
    };
    // Eagerly consume tombstones the cursor stopped flush against (see
    // `bucket::advance_cursor`).
    while (!bucket.closed.is_empty() && bucket.closed[0].start == cur) {
        let PutClosedInterval { start, end } = bucket.closed.remove(0);
        bucket.closed_pending = bucket.closed_pending - (end - start);
        cur = end;
    };
    bucket.exercise_cursor = cur;
}

/// The start of the first live spread range the cursor has not fully
/// passed (`end > cur`), or `total_written` if none — ranges the cursor
/// exited stay in the vector until closed/redeemed but no longer bound
/// the plain path.
fun next_spread_start<U, S, P>(bucket: &PutBucket<U, S, P>, cur: u128): u128 {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        if (bucket.spreads[i].end > cur) {
            return bucket.spreads[i].start
        };
        i = i + 1;
    };
    bucket.total_written
}

fun overlaps_spread<U, S, P>(bucket: &PutBucket<U, S, P>, start: u128, end: u128): bool {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        let sp = &bucket.spreads[i];
        if (sp.start < end && start < sp.end) {
            return true
        };
        i = i + 1;
    };
    false
}

fun find_spread_exact<U, S, P>(bucket: &PutBucket<U, S, P>, start: u128, end: u128): u64 {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        if (bucket.spreads[i].start == start) {
            assert!(bucket.spreads[i].end == end, errors::spread_not_found());
            return i
        };
        i = i + 1;
    };
    abort errors::spread_not_found()
}

/// Sorted-insert with adjacency merging; mirror of `bucket::insert_closed`.
fun insert_closed<U, S, P>(bucket: &mut PutBucket<U, S, P>, start: u128, end: u128) {
    let mut i = 0;
    while (i < bucket.closed.length() && bucket.closed[i].start < start) {
        i = i + 1;
    };
    bucket.closed.insert(PutClosedInterval { start, end }, i);
    if (i + 1 < bucket.closed.length() && bucket.closed[i].end == bucket.closed[i + 1].start) {
        let PutClosedInterval { start: _, end: right_end } = bucket.closed.remove(i + 1);
        bucket.closed[i].end = right_end;
    };
    if (i > 0 && bucket.closed[i - 1].end == bucket.closed[i].start) {
        let PutClosedInterval { start: _, end: cur_end } = bucket.closed.remove(i);
        bucket.closed[i - 1].end = cur_end;
    };
    bucket.closed_pending = bucket.closed_pending + (end - start);
}

// ─────────────────── put spread collateral compression ───────────────────
//
// A short put backed by an escrowed LONG put plus the strike-gap cash
// instead of the full `ceil(amount × strike)` collateral. Physical
// settlement makes assignment special: the cash owed to an exerciser can
// only come from exercising the escrowed long — WITH THE UNDERLYING THE
// ASSIGNMENT ITSELF DELIVERS. There is therefore no pre-physicalization
// crank (the call side's `unwind_spread`); instead the unwind is fused
// into a spread-aware exercise entry (`exercise_spread`), and the plain
// path refuses to carry the cursor into a compressed range.
//
// Escrow sizing and the partial-assignment ledger: with cumulative
// assigned units a, the exerciser payout TARGET telescopes on the short
// leg — pay(a₀→a₁) = ceil(a₁·Ks) − ceil(a₀·Ks) — so total payouts are
// exact-aggregate regardless of chunking (within one unit of the plain
// `floor(n·Ks)` per exercise, holder-favoring). Funding per chunk is the
// REAL long proceeds `floor(n·Kl)`: any excess over the target is the
// writer's surplus and joins the escrow cash; any deficit draws from the
// escrow cash, clamped to what it holds. The escrow is provisioned with
//   g(A) + 1,   g(a) = max(ceil(a·Ks) − floor(a·Kl), 0),
// which covers the aggregate gap; per-chunk floor losses on the long
// proceeds (≤ 1 unit each) are borne by the payout clamp, never by the
// pool — the design is solvent by construction, with dust-bounded payout
// deviation, the same aggregate-rounding posture the cash leg documents.
//
// The short bucket's pooled `settlement_balance` is never touched by
// spread flows, so the module-doc solvency proof is unaffected.

/// The gap cash required to escrow `amount` units: g(amount) + 1.
public fun required_spread_top_up<U, S, P, LU, LS, LP>(
    bucket: &PutBucket<U, S, P>,
    long_bucket: &PutBucket<LU, LS, LP>,
    amount: u64,
): u64 {
    spread_cash_need(
        amount as u128,
        bucket.strike,
        bucket.strike_scale,
        long_bucket.strike,
        long_bucket.strike_scale,
    ) + 1
}

/// g(a): the escrow cash needed once `a` units are assigned.
fun spread_cash_need(a: u128, strike: u128, scale: u8, l_strike: u128, l_scale: u8): u64 {
    let need = apply_strike_ceil(a, strike, scale);
    let have = apply_strike_floor(a, l_strike, l_scale);
    if (need > have) { need - have } else { 0 }
}

/// Write `long.value()` units backed by escrowed long puts plus exactly
/// `required_spread_top_up` gap cash. The long leg must be exercisable
/// whenever this bucket is (equal-or-later expiry); any strike works —
/// the gap cash covers the difference (zero gap + cushion when the long
/// strike is equal-or-higher).
public fun write_spread<Underlying, Settlement, Put, LongPut>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    wl: &Whitelist,
    long_bucket: &PutBucket<Underlying, Settlement, LongPut>,
    long: Coin<LongPut>,
    top_up: Coin<Settlement>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    let amount = long.value();
    assert!(amount > 0, errors::zero_amount());
    assert!(long_bucket.expiry_ms >= bucket.expiry_ms, errors::spread_expiry_mismatch());
    assert!(
        top_up.value() == required_spread_top_up(bucket, long_bucket, amount),
        errors::settlement_amount_mismatch(),
    );

    let range_start = bucket.total_written;
    let range_end = range_start + (amount as u128);
    bucket.total_written = range_end;
    bucket.spreads.push_back(PutSpreadRange { start: range_start, end: range_end });
    let top_up_amount = top_up.value();
    df::add(
        &mut bucket.id,
        PutSpreadEscrowKey { start: range_start },
        PutSpreadEscrow<LongPut, Settlement> {
            long: long.into_balance(),
            cash: top_up.into_balance(),
            long_bucket_id: object::id(long_bucket),
        },
    );

    let position = position::mint(object::id(bucket), range_start, range_end, ctx);
    let put = coin::mint(&mut bucket.put_treasury, amount, ctx);
    events::emit_put_spread_written(
        object::id(bucket),
        object::id(long_bucket),
        ctx.sender(),
        object::id(&position),
        amount,
        top_up_amount,
        range_start,
        range_end,
    );
    (position, put)
}

/// Exercise into the compressed range at the cursor: burn the puts,
/// route the delivered underlying through the escrowed long's exercise,
/// and pay out the proceeds plus the telescoped escrow draw. The plain
/// `exercise` aborts `put_spread_exercise_required` at a spread range —
/// this is the entry that carries the cursor through it.
public fun exercise_spread<Underlying, Settlement, Put, LongPut>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    long_bucket: &mut PutBucket<Underlying, Settlement, LongPut>,
    put: Coin<Put>,
    underlying_delivery: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Settlement> {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let amount = put.value();
    assert!(amount > 0, errors::zero_amount());
    assert!(underlying_delivery.value() == amount, errors::amount_mismatch());

    let cur = bucket.exercise_cursor;
    let range_start = next_spread_start(bucket, cur);
    // The cursor must have reached the range (plain capacity before it
    // exercises via the plain path first) …
    assert!(range_start <= cur, errors::put_spread_not_at_cursor());
    let idx = find_spread(bucket, range_start);
    let range_end = bucket.spreads[idx].end;
    // … and the exercise must stay inside it (split across the boundary).
    assert!(cur + (amount as u128) <= range_end, errors::cursor_overflow());

    coin::burn(&mut bucket.put_treasury, put);

    let a0 = cur - range_start;
    let a1 = a0 + (amount as u128);
    let strike = bucket.strike;
    let strike_scale = bucket.strike_scale;
    let bucket_id = object::id(bucket);

    let escrow: &mut PutSpreadEscrow<LongPut, Settlement> =
        df::borrow_mut(&mut bucket.id, PutSpreadEscrowKey { start: range_start });
    assert!(escrow.long_bucket_id == object::id(long_bucket), errors::spread_bucket_mismatch());
    let long_bucket_id = escrow.long_bucket_id;
    let long_coins = coin::from_balance(escrow.long.split(amount), ctx);

    // Telescoped short-leg payout target: exact in aggregate however the
    // range is chunked.
    let target = apply_strike_ceil(a1, strike, strike_scale)
        - apply_strike_ceil(a0, strike, strike_scale);

    // The delivered underlying exercises the escrowed long; proceeds
    // above the target are the writer's surplus (escrowed), a deficit
    // draws from the escrow cash (clamped — see the section comment).
    let proceeds = exercise(long_bucket, long_coins, underlying_delivery, clock, ctx);
    let mut pay = proceeds.into_balance();
    let have = pay.value();
    if (have > target) {
        escrow.cash.join(pay.split(have - target));
    } else if (have < target) {
        let deficit = target - have;
        let take = if (deficit > escrow.cash.value()) { escrow.cash.value() } else { deficit };
        pay.join(escrow.cash.split(take));
    };

    // Carry the cursor through the assigned slice; flush any tombstones
    // it lands against (none can exist inside the range itself).
    let mut new_cur = cur + (amount as u128);
    while (!bucket.closed.is_empty() && bucket.closed[0].start == new_cur) {
        let PutClosedInterval { start, end } = bucket.closed.remove(0);
        bucket.closed_pending = bucket.closed_pending - (end - start);
        new_cur = end;
    };
    bucket.exercise_cursor = new_cur;

    let payout = pay.value();
    events::emit_put_spread_exercised(
        bucket_id,
        long_bucket_id,
        ctx.sender(),
        amount,
        payout,
        new_cur,
    );
    coin::from_balance(pay, ctx)
}

/// Pre-expiry retirement of a spread position: burn back the UNASSIGNED
/// remainder's coins, tombstone that remainder, and return the whole
/// escrow (remaining longs + cash — including any assignment surplus).
/// The assigned slice already settled at exercise time, so the whole
/// range counts as redeemed and the position is consumed.
public fun close_spread<Underlying, Settlement, Put, LongPut>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    position: Position,
    put: Coin<Put>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<LongPut>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == object::id(bucket), errors::position_bucket_mismatch());
    let idx = find_spread_exact(bucket, rs, re);

    let cur = bucket.exercise_cursor;
    let assigned_end = if (cur <= rs) { rs } else if (cur >= re) { re } else { cur };
    let remaining = re - assigned_end;
    assert!((put.value() as u128) == remaining, errors::amount_mismatch());
    coin::burn(&mut bucket.put_treasury, put);
    if (remaining > 0) {
        insert_closed(bucket, assigned_end, re);
    };
    bucket.total_redeemed = bucket.total_redeemed + (re - rs);
    bucket.spreads.remove(idx);
    let PutSpreadEscrow<LongPut, Settlement> { long, cash, long_bucket_id: _ } =
        df::remove(&mut bucket.id, PutSpreadEscrowKey { start: rs });
    events::emit_put_spread_closed(
        object::id(bucket),
        ctx.sender(),
        position_id,
        rs,
        re,
        remaining as u64,
    );
    (coin::from_balance(long, ctx), coin::from_balance(cash, ctx))
}

/// Post-expiry exit for a spread position: the remaining escrow
/// (unassigned longs + cash, including assignment surplus) back to the
/// holder. The unassigned coins expire worthless as usual
/// (`burn_expired_option`).
public fun redeem_spread_position<Underlying, Settlement, Put, LongPut>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    position: Position,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<LongPut>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == object::id(bucket), errors::position_bucket_mismatch());
    let idx = find_spread_exact(bucket, rs, re);
    bucket.total_redeemed = bucket.total_redeemed + (re - rs);
    bucket.spreads.remove(idx);
    let PutSpreadEscrow<LongPut, Settlement> { long, cash, long_bucket_id: _ } =
        df::remove(&mut bucket.id, PutSpreadEscrowKey { start: rs });
    events::emit_put_spread_redeemed(
        object::id(bucket),
        ctx.sender(),
        position_id,
        rs,
        re,
        (re - rs) as u64,
    );
    (coin::from_balance(long, ctx), coin::from_balance(cash, ctx))
}

fun find_spread<U, S, P>(bucket: &PutBucket<U, S, P>, start: u128): u64 {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        if (bucket.spreads[i].start == start) {
            return i
        };
        i = i + 1;
    };
    abort errors::spread_not_found()
}

/// Live (not yet closed/redeemed) compressed ranges.
public fun spread_count<U, S, P>(bucket: &PutBucket<U, S, P>): u64 { bucket.spreads.length() }

/// Does [start, end) overlap any live compressed range?
public fun range_overlaps_spread<U, S, P>(
    bucket: &PutBucket<U, S, P>,
    start: u128,
    end: u128,
): bool {
    overlaps_spread(bucket, start, end)
}

/// Escrow view for the live compressed range exactly [start, end):
/// (remaining long units, escrow cash, long bucket id).
public fun spread_escrow_view<U, S, P, LongPut>(
    bucket: &PutBucket<U, S, P>,
    start: u128,
    end: u128,
): (u64, u64, ID) {
    find_spread_exact(bucket, start, end);
    let escrow: &PutSpreadEscrow<LongPut, S> =
        df::borrow(&bucket.id, PutSpreadEscrowKey { start });
    (escrow.long.value(), escrow.cash.value(), escrow.long_bucket_id)
}

public fun burn_expired_option<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    put: Coin<Put>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let bucket_id = object::id(bucket);
    let amount = coin::burn(&mut bucket.put_treasury, put);
    events::emit_put_expired_option_burned(bucket_id, ctx.sender(), amount);
}

#[allow(lint(self_transfer))]
public fun cleanup_bucket<Underlying, Settlement, Put>(
    _: &AdminCap,
    bucket: PutBucket<Underlying, Settlement, Put>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let PutBucket {
        id,
        asset_type: _,
        settlement_type: _,
        put_type: _,
        expiry_ms: _,
        strike: _,
        strike_scale: _,
        total_written,
        exercise_cursor: _,
        total_redeemed,
        underlying_balance,
        settlement_balance,
        put_treasury,
        invalidated: _,
        closed: _,
        closed_pending: _,
        spreads,
    } = bucket;
    // Every spread range must be closed or redeemed (their escrows are
    // dynamic fields on the bucket — none may remain).
    assert!(spreads.is_empty(), errors::bucket_not_drained());
    // Every position must be redeemed before cleanup, so the only cash left
    // is rounding dust (never an unredeemed writer's collateral).
    assert!(total_redeemed == total_written, errors::bucket_not_drained());
    assert!(underlying_balance.value() == 0, errors::bucket_not_drained());
    underlying_balance.destroy_zero();

    // Sweep the rounding remainder to the admin rather than forcing it to
    // zero (a `== 0` gate would be unreachable for fractional strikes).
    let dust = settlement_balance.value();
    if (dust > 0) {
        transfer::public_transfer(coin::from_balance(settlement_balance, ctx), ctx.sender());
    } else {
        settlement_balance.destroy_zero();
    };
    // Outstanding put coins may still exist; hand the cap back to the admin.
    transfer::public_transfer(put_treasury, ctx.sender());
    let bucket_id = id.to_inner();
    id.delete();
    events::emit_put_bucket_cleaned(bucket_id, dust);
}

public fun invalidate_bucket<Underlying, Settlement, Put>(
    _: &AdminCap,
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    bucket.invalidated = true;
    events::emit_put_bucket_invalidated(object::id(bucket), now, ctx.sender(), reason);
}

public fun revalidate_bucket<Underlying, Settlement, Put>(
    _: &AdminCap,
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(bucket.invalidated, errors::bucket_not_invalidated());
    bucket.invalidated = false;
    events::emit_put_bucket_revalidated(object::id(bucket), now, ctx.sender(), reason);
}

// ---- getters ----

public fun expiry_ms<U, S, P>(bucket: &PutBucket<U, S, P>): u64 { bucket.expiry_ms }
public fun invalidated<U, S, P>(bucket: &PutBucket<U, S, P>): bool { bucket.invalidated }
public fun strike<U, S, P>(bucket: &PutBucket<U, S, P>): u128 { bucket.strike }
public fun strike_scale<U, S, P>(bucket: &PutBucket<U, S, P>): u8 { bucket.strike_scale }
public fun total_written<U, S, P>(bucket: &PutBucket<U, S, P>): u128 { bucket.total_written }
public fun exercise_cursor<U, S, P>(bucket: &PutBucket<U, S, P>): u128 { bucket.exercise_cursor }
public fun total_redeemed<U, S, P>(bucket: &PutBucket<U, S, P>): u128 { bucket.total_redeemed }
public fun asset_type<U, S, P>(bucket: &PutBucket<U, S, P>): TypeName { bucket.asset_type }
public fun settlement_type<U, S, P>(bucket: &PutBucket<U, S, P>): TypeName { bucket.settlement_type }
public fun put_type<U, S, P>(bucket: &PutBucket<U, S, P>): TypeName { bucket.put_type }

/// Units tombstoned by offset closure not yet jumped by the cursor.
public fun closed_pending<U, S, P>(bucket: &PutBucket<U, S, P>): u128 { bucket.closed_pending }

public fun put_supply<U, S, P>(bucket: &PutBucket<U, S, P>): u64 {
    coin::total_supply(&bucket.put_treasury)
}

public fun underlying_balance<U, S, P>(bucket: &PutBucket<U, S, P>): u64 {
    bucket.underlying_balance.value()
}

public fun settlement_balance<U, S, P>(bucket: &PutBucket<U, S, P>): u64 {
    bucket.settlement_balance.value()
}

#[test_only]
public fun apply_strike_ceil_for_testing(amount: u128, strike: u128, strike_scale: u8): u64 {
    apply_strike_ceil(amount, strike, strike_scale)
}

#[test_only]
public fun apply_strike_floor_for_testing(amount: u128, strike: u128, strike_scale: u8): u64 {
    apply_strike_floor(amount, strike, strike_scale)
}
