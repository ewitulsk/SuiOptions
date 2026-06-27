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
module options_protocol::put_bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};

use options_protocol::account::{Self, Account};
use options_protocol::admin::{AdminCap, ProtocolConfig};
use options_protocol::bucket::{Self, FlowKind};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position::{Self, Position};
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::treasury::Treasury;

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
public(package) fun required_collateral<U, S, P>(bucket: &PutBucket<U, S, P>, amount: u64): u64 {
    apply_strike_ceil(amount as u128, bucket.strike, bucket.strike_scale)
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

public fun execute_write<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    collateral_in: Coin<Settlement>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    put_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_and_consume_quote(signer_account, config, &signed_quote, clock);
    execute_write_with_quote<Underlying, Settlement, Put>(
        bucket,
        config,
        treasury,
        signer_account,
        collateral_in,
        premium_in,
        flow,
        position_recipient,
        put_token_recipient,
        q,
        clock,
        ctx,
    );
}

#[test_only]
public fun execute_write_for_testing<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    collateral_in: Coin<Settlement>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    put_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_skip_sig(signer_account, config, &signed_quote, clock);
    execute_write_with_quote<Underlying, Settlement, Put>(
        bucket,
        config,
        treasury,
        signer_account,
        collateral_in,
        premium_in,
        flow,
        position_recipient,
        put_token_recipient,
        q,
        clock,
        ctx,
    );
}

#[allow(lint(self_transfer))]
fun execute_write_with_quote<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    collateral_in: Coin<Settlement>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    put_token_recipient: address,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());
    let collateral_required = required_collateral(bucket, write_amount);

    let (collateral, fee) = if (bucket::is_writer(&flow)) {
        // Writer flow: signer is the trader MM — the BUYER of the put.
        // Signer-supplied side: premium (Settlement) debited from their
        // Account. Executor-supplied side: cash collateral matching
        // ceil(write_amount × strike). The executor (the writer) keeps the
        // net premium; the buyer gets the put coins.
        assert!(signer_recipient == put_token_recipient, errors::quote_recipient_mismatch());
        assert!(premium_in.value() == 0, errors::amount_mismatch());
        assert!(collateral_in.value() == collateral_required, errors::put_collateral_mismatch());

        let premium_coin = account::withdraw_internal<Settlement>(
            signer_account,
            gross_premium,
            ctx,
        );
        let (net_balance, fee) = bucket::skim_fee(config, treasury, premium_coin.into_balance());
        let net_coin = coin::from_balance(net_balance, ctx);
        transfer::public_transfer(net_coin, ctx.sender());

        premium_in.destroy_zero();
        (collateral_in.into_balance(), fee)
    } else {
        // Trader flow: signer is the writer MM — the SELLER/writer of the
        // put. Signer-supplied side: cash collateral debited from their
        // Account. Executor-supplied side: premium matching gross_premium.
        // The signer (writer) keeps the net premium; the executor (the
        // buyer) gets the put coins.
        assert!(signer_recipient == position_recipient, errors::quote_recipient_mismatch());
        assert!(collateral_in.value() == 0, errors::amount_mismatch());
        assert!(premium_in.value() == gross_premium, errors::amount_mismatch());

        let collateral_coin = account::withdraw_internal<Settlement>(
            signer_account,
            collateral_required,
            ctx,
        );
        let (net_balance, fee) = bucket::skim_fee(config, treasury, premium_in.into_balance());
        account::deposit_balance(signer_account, net_balance);

        collateral_in.destroy_zero();
        (collateral_coin.into_balance(), fee)
    };
    let net_premium = gross_premium - fee;

    let (position, put) = do_write(bucket, collateral, write_amount, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(put, put_token_recipient);

    events::emit_put_write_executed(
        bucket_id,
        quote::signer_account_id(&q),
        signer_recipient,
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

/// Core cash-secured write: escrow `collateral_in` cash in the bucket and
/// mint the `Position` + `Coin<Put>`, returned to the caller (no transfers).
/// Mirrors `bucket::write_collateralized`; safe to expose `public` for the
/// same reason — the caller fully collateralizes every unit minted and holds
/// both sides until they part with the put coin.
public fun write_collateralized<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    collateral_in: Coin<Settlement>,
    write_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
    write_collateralized_balance(bucket, collateral_in.into_balance(), write_amount, clock, ctx)
}

/// `Balance`-accepting sibling for in-package venues (the put RFQ) whose
/// escrow lives as a `Balance`.
public(package) fun write_collateralized_balance<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    collateral: Balance<Settlement>,
    write_amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Put>) {
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
        bucket.exercise_cursor + (amount as u128) <= bucket.total_written,
        errors::cursor_overflow(),
    );

    // Burning through the bucket's own treasury enforces, by type, that the
    // coin belongs to this bucket.
    coin::burn(&mut bucket.put_treasury, put);

    bucket.underlying_balance.join(underlying_delivery.into_balance());
    bucket.exercise_cursor = bucket.exercise_cursor + (amount as u128);

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
    } = bucket;
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
