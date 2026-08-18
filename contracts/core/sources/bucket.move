module options_core::bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};
use sui::dynamic_field as df;

use sui::coin_registry::CoinRegistry;

use options_core::admin::{Self, AdminCap, ProtocolConfig};
use options_core::bucket_registry::{Self, BucketRegistry};
use options_core::option_coin::{Self, OptionCall};
use whitelist::whitelist::{Self, Whitelist};
use options_core::collateral::{Self, CollateralRequest};
use options_core::errors;
use options_core::events;
use options_core::position::{Self, Position};
use options_core::quote::{Self, Quote, SignedQuote};
use options_core::quote_signer::QuoteSigner;
use options_core::treasury::{Self, Treasury};

/// The call option is a per-bucket fungible coin: `Coin<Call>`. `Call` is a
/// One-Time-Witness type minted from a package the options-scheduler
/// publishes per bucket set, so every bucket has its own coin currency. The
/// bucket owns the sole `TreasuryCap<Call>` — it is the only minter and
/// burner — which makes the coin's outstanding supply exactly equal to the
/// outstanding (unexercised, unburned) option amount, and makes bucket
/// isolation a type-system guarantee rather than a runtime `bucket_id` check.
public struct Bucket<phantom Underlying, phantom Settlement, phantom Call> has key {
    id: UID,
    asset_type: TypeName,
    settlement_type: TypeName,
    call_type: TypeName,
    expiry_ms: u64,
    /// Strike ratio in scaled chain units. The real ratio (settlement
    /// smallest-units per underlying smallest-unit) is
    /// `strike / 10^strike_scale`. Using u128 + u8 lets a sub-cent asset
    /// paired against a same-decimal stablecoin (both 6-dec, say) carry
    /// meaningful resolution that a plain integer ratio cannot.
    strike: u128,
    strike_scale: u8,
    total_written: u128,
    exercise_cursor: u128,
    underlying_balance: Balance<Underlying>,
    settlement_balance: Balance<Settlement>,
    /// Sole mint/burn authority for the option coin. Held for the bucket's
    /// whole life; never exposed by reference outside this module.
    call_treasury: TreasuryCap<Call>,
    /// Admin-controlled freeze on new writes. Exercises and redeems are
    /// unaffected — invalidation only blocks `execute_write`. Toggleable
    /// pre-expiry via `invalidate_bucket` / `revalidate_bucket`.
    invalidated: bool,
    /// Exact-offset closure tombstones: ranges whose writer burned equal
    /// option coins and reclaimed collateral. Sorted ascending, disjoint,
    /// every interval ≥ `exercise_cursor`; the cursor skips them (they hold
    /// no collateral and their coins are burned). See `close_offset`.
    closed: vector<ClosedInterval>,
    /// Total units across `closed` — subtracted from exercise capacity.
    closed_pending: u128,
    /// Compressed (spread) write ranges: backed by an escrowed long call +
    /// its exercise cash instead of underlying. The cursor may not enter one
    /// until `unwind_spread` physicalizes it. Sorted ascending, disjoint,
    /// every range ≥ `exercise_cursor`. See `write_spread`.
    spreads: vector<SpreadRange>,
}

/// A tombstoned (offset-closed) slice of the write space.
public struct ClosedInterval has copy, drop, store { start: u128, end: u128 }

/// A spread-compressed slice of the write space; its escrow lives as a
/// dynamic field under `SpreadEscrowKey { start }`.
public struct SpreadRange has copy, drop, store { start: u128, end: u128 }

public struct SpreadEscrowKey has copy, drop, store { start: u128 }

/// The collateral backing a compressed write: a long call on the same
/// (Underlying, Settlement) pair at an equal-or-lower strike and an
/// equal-or-later expiry, plus exactly the cash needed to exercise it.
public struct SpreadEscrow<phantom LongCall, phantom Settlement> has store {
    long: Balance<LongCall>,
    cash: Balance<Settlement>,
    long_bucket_id: ID,
}

/// Maximum supported strike_scale. 38 is the largest exponent for which
/// `pow10` still fits in u128 (`10^38 ≈ 1×10^38`, `u128::MAX ≈ 3.4×10^38`);
/// passing 39 would abort inside the loop's multiply, so we cap one below
/// that on a dedicated assert for a cleaner error.
const MAX_STRIKE_SCALE: u8 = 38;

/// 10^exp for exp ∈ [0, MAX_STRIKE_SCALE]. Aborts if exp exceeds the cap
/// — keeps `pow10` cheap and guarantees the result fits in u128.
///
/// `public(package)` so the cash-secured-put module reuses the exact same
/// power-of-ten table (and its overflow guard) rather than carrying a copy
/// that could drift.
public(package) fun pow10(exp: u8): u128 {
    assert!(exp <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    let mut result: u128 = 1;
    let mut i: u8 = 0;
    while (i < exp) {
        result = result * 10;
        i = i + 1;
    };
    result
}

/// settlement = round_half_up((amount × strike) / 10^strike_scale).
///
/// Round-half-up (not floor) so a tiny exercise rounds to the nearest
/// settlement smallest-unit instead of consistently truncating to zero
/// in the buyer's favor. Aborts via the u64 cast if the result exceeds
/// u64::MAX (Coin<T>::value is u64).
fun apply_strike(amount: u128, strike: u128, strike_scale: u8): u64 {
    let divisor = pow10(strike_scale);
    let numerator = amount * strike;
    let half = divisor / 2;
    ((numerator + half) / divisor) as u64
}

/// Buckets are created only through the permissionless any-strike path
/// below. The AdminCap creator that used to live here — which took a
/// pre-published `TreasuryCap<Call>` and a raw, UN-normalized strike — was
/// removed with SO-408: quotes now bind a bucket's spec rather than its
/// object id, which is only sound while `bucket_registry` admits exactly one
/// bucket per spec. A second creation path could mint a duplicate with the
/// same economics and a different exercise queue, and a signed quote would
/// match both.

#[test_only]
/// Share a bucket for an arbitrary `(U, S, Call)` triple, bypassing the
/// coin-registry machinery so the suite can keep using plain marker coin
/// types. Test-only on purpose: `create_bucket_any_strike` is the sole
/// on-chain creation path, which is what makes one-bucket-per-spec — and so
/// spec-bound quoting — sound.
public fun create_bucket_for_testing<Underlying, Settlement, Call>(
    call_treasury: TreasuryCap<Call>,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
    ctx: &mut TxContext,
) {
    assert!(strike_scale <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    assert!(coin::total_supply(&call_treasury) == 0, errors::treasury_cap_not_fresh());

    let asset_type = type_name::with_defining_ids<Underlying>();
    let settlement_type = type_name::with_defining_ids<Settlement>();
    let call_type = type_name::with_defining_ids<Call>();
    let bucket = Bucket<Underlying, Settlement, Call> {
        id: object::new(ctx),
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        strike,
        strike_scale,
        total_written: 0,
        exercise_cursor: 0,
        underlying_balance: balance::zero<Underlying>(),
        settlement_balance: balance::zero<Settlement>(),
        call_treasury,
        invalidated: false,
        closed: vector[],
        closed_pending: 0,
        spreads: vector[],
    };
    events::emit_bucket_created(
        object::id(&bucket),
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        strike,
        strike_scale,
    );
    transfer::share_object(bucket);
}

/// Any-strike permissionless creation: register this instantiation's coin
/// currency at runtime (`option_coin::register_call` — aborts unless the
/// marker parameters spell exactly the normalized economics, and aborts if
/// the currency already exists), claim the bucket's `UID` from the derived
/// registry (deterministic ID, second dedup guard), and return the bucket
/// BY VALUE. The bucket has `key` only, and this module exposes no other
/// consumer — so the creating transaction MUST end with `share_bucket`,
/// letting the same PTB thread the fresh bucket through quote/write calls
/// first. Ingress-gated like every write venue.
public fun create_bucket_any_strike<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(
    registry: &mut BucketRegistry,
    coin_registry: &mut CoinRegistry,
    wl: &Whitelist,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
    coin_decimals: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): Bucket<U, S, OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>> {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(clock.timestamp_ms() < expiry_ms, errors::bucket_expired());
    // Minute-aligned expiries keep the u32-minutes type encoding injective.
    assert!(expiry_ms % 60_000 == 0, errors::expiry_not_aligned());
    let expiry_minutes = expiry_ms / 60_000;
    assert!(expiry_minutes <= (std::u32::max_value!() as u64), errors::expiry_not_aligned());
    assert!(strike_scale <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    let (sig, exp) = option_coin::normalize_strike(strike, strike_scale);

    let asset_type = type_name::with_defining_ids<U>();
    let settlement_type = type_name::with_defining_ids<S>();
    let id = sui::derived_object::claim(
        bucket_registry::uid_mut(registry),
        bucket_registry::key(asset_type, settlement_type, expiry_ms, sig, exp, /* is_put */ false),
    );
    let call_treasury = option_coin::register_call<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(coin_registry, expiry_minutes as u32, sig, exp, coin_decimals, ctx);

    let call_type = type_name::with_defining_ids<
        OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>,
    >();
    let bucket = Bucket<U, S, OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>> {
        id,
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        // Stored normalized: identical economics to the raw input, and the
        // bucket's on-chain identity matches its coin-type encoding.
        strike: sig as u128,
        strike_scale: exp,
        total_written: 0,
        exercise_cursor: 0,
        underlying_balance: balance::zero<U>(),
        settlement_balance: balance::zero<S>(),
        call_treasury,
        invalidated: false,
        closed: vector[],
        closed_pending: 0,
        spreads: vector[],
    };
    events::emit_bucket_created(
        object::id(&bucket),
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        sig as u128,
        exp,
    );
    bucket
}

/// Terminal command of every `create_bucket_any_strike` transaction.
#[allow(lint(share_owned))]
public fun share_bucket<U, S, C>(bucket: Bucket<U, S, C>) {
    transfer::share_object(bucket);
}

/// Writer flow, step 1 of the collateral protocol (see `collateral.move`):
/// the signer is the trader MM (the buyer). Verifies the signed quote
/// (consuming its nonce) against this bucket and demands the MM's PREMIUM
/// as a `CollateralRequest<Settlement>` for their `release` implementation
/// to fulfill. Consumed by `execute_writer_flow` in the same transaction.
public fun request_writer_flow<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_and_consume_quote(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    let premium = quote::premium(&q);
    collateral::new_writer_request<Settlement>(q, premium, object::id(bucket))
}

/// Trader flow, step 1: the signer is the writer MM (the seller). Demands
/// the MM's UNDERLYING write collateral as a `CollateralRequest<Underlying>`.
public fun request_trader_flow<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Underlying> {
    let q = quote::verify_and_consume_quote(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    let amount = quote::write_amount(&q);
    collateral::new_trader_request<Underlying>(q, amount, object::id(bucket))
}

/// The quote's signed spec must describe THIS bucket.
///
/// Rebuilding the expected key through `bucket_registry::key` — the same
/// constructor the registry derives the bucket's address from — is what keeps
/// the two from drifting: if `BucketKey` ever gains a field, this check picks
/// it up and any quote that does not carry it stops compiling.
///
/// The strike is re-normalized rather than compared raw so the check does not
/// depend on how the bucket was created.
fun assert_quote_bucket<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    q: &Quote,
    clock: &Clock,
) {
    assert_quote_spec(bucket, q);
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    assert!(quote::write_amount(q) > 0, errors::zero_amount());
}

/// Spec + queue-bound check, shared by the request and execute legs.
fun assert_quote_spec<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    q: &Quote,
) {
    let (sig, exp) = option_coin::normalize_strike(bucket.strike, bucket.strike_scale);
    let expected = bucket_registry::key(
        bucket.asset_type,
        bucket.settlement_type,
        bucket.expiry_ms,
        sig,
        exp,
        /* is_put */ false,
    );
    assert!(*quote::spec(q) == expected, errors::quote_spec_mismatch());
    assert!(
        bucket.total_written <= quote::max_total_written(q),
        errors::quote_queue_exceeded(),
    );
}

/// Writer flow, step 2: consume the potato + the released premium and
/// execute the covered write. The executor (the retail writer, tx sender)
/// supplies the underlying and receives the `Position` + net premium; the
/// MM's `Coin<Call>` goes to the quote's `signer_token_recipient`.
#[allow(lint(self_transfer))]
public fun execute_writer_flow<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    wl: &Whitelist,
    treasury: &mut Treasury,
    request: CollateralRequest<Settlement>,
    premium_funds: Balance<Settlement>,
    underlying_in: Coin<Underlying>,
    position_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    let (q, amount, is_writer) = collateral::destroy(request);
    assert!(is_writer, errors::request_flow_mismatch());
    let bucket_id = object::id(bucket);
    // Re-checked here, not just at request time: the potato only proves a
    // quote was verified, not which bucket it is being spent against.
    assert_quote_spec(bucket, &q);
    assert!(premium_funds.value() == amount, errors::amount_mismatch());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let call_token_recipient = quote::signer_token_recipient(&q);
    assert!(underlying_in.value() == write_amount, errors::amount_mismatch());

    let (net_balance, fee) = skim_fee(config, treasury, premium_funds);
    let net_premium = gross_premium - fee;
    transfer::public_transfer(coin::from_balance(net_balance, ctx), ctx.sender());

    let (position, call) = write_and_check(bucket, underlying_in.into_balance(), clock, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(call, call_token_recipient);

    events::emit_write_executed(
        bucket_id,
        quote::signer_id(&q),
        quote::collateral_source(&q),
        call_token_recipient,
        ctx.sender(),
        position_id,
        position_recipient,
        call_token_recipient,
        write_amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );
}

/// Trader flow, step 2: consume the potato + the released underlying and
/// execute the covered write. The executor (the retail trader, tx sender)
/// pays the premium and chooses where the `Coin<Call>` goes; the MM
/// receives the `Position` + net premium at `signer_token_recipient`.
public fun execute_trader_flow<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    wl: &Whitelist,
    treasury: &mut Treasury,
    request: CollateralRequest<Underlying>,
    underlying_funds: Balance<Underlying>,
    premium_in: Coin<Settlement>,
    call_token_recipient: address,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    let (q, amount, is_writer) = collateral::destroy(request);
    assert!(!is_writer, errors::request_flow_mismatch());
    let bucket_id = object::id(bucket);
    // See the writer-flow twin: the potato does not name a bucket.
    assert_quote_spec(bucket, &q);
    assert!(underlying_funds.value() == amount, errors::amount_mismatch());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let position_recipient = quote::signer_token_recipient(&q);
    assert!(underlying_funds.value() == write_amount, errors::amount_mismatch());
    assert!(premium_in.value() == gross_premium, errors::amount_mismatch());

    let (net_balance, fee) = skim_fee(config, treasury, premium_in.into_balance());
    let net_premium = gross_premium - fee;
    transfer::public_transfer(coin::from_balance(net_balance, ctx), position_recipient);

    let (position, call) = write_and_check(bucket, underlying_funds, clock, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(call, call_token_recipient);

    events::emit_write_executed(
        bucket_id,
        quote::signer_id(&q),
        quote::collateral_source(&q),
        position_recipient,
        ctx.sender(),
        position_id,
        position_recipient,
        call_token_recipient,
        write_amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );
}

/// Shared execute-step bucket checks: the request was minted against a
/// live bucket, but re-assert cheaply in case state changed inside the
/// same transaction (an earlier PTB command could not have expired it,
/// but invalidation gating is defense-in-depth).
fun write_and_check<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    underlying: Balance<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    do_write(bucket, underlying, ctx)
}

/// Test-only request twins that skip signature verification (test quotes
/// carry empty signatures); every other check is identical.
#[test_only]
public fun request_writer_flow_for_testing<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Settlement> {
    let q = quote::verify_skip_sig(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    let premium = quote::premium(&q);
    collateral::new_writer_request<Settlement>(q, premium, object::id(bucket))
}

#[test_only]
public fun request_trader_flow_for_testing<Underlying, Settlement, Call>(
    bucket: &Bucket<Underlying, Settlement, Call>,
    signer: &mut QuoteSigner,
    config: &ProtocolConfig,
    signed_quote: SignedQuote,
    clock: &Clock,
): CollateralRequest<Underlying> {
    let q = quote::verify_skip_sig(signer, config, &signed_quote, clock);
    assert_quote_bucket(bucket, &q, clock);
    let amount = quote::write_amount(&q);
    collateral::new_trader_request<Underlying>(q, amount, object::id(bucket))
}

/// Core covered-write: escrow `underlying_in` in the bucket and mint the
/// corresponding `Position` + `Coin<Call>`, returned to the caller (no
/// transfers). Premium negotiation is a venue-layer concern.
///
/// Safe to expose `public`: this mints no free optionality. The caller
/// fully collateralizes every option unit minted, and until they part with
/// the `Coin<Call>` they hold both sides of the trade — economically a
/// no-op. It is exactly the "self-write" primitive that lets anyone build
/// a venue (auction, AMM listing, OTC) on top of the protocol.
public fun write_collateralized<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    wl: &Whitelist,
    underlying_in: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    write_collateralized_balance(bucket, wl, underlying_in.into_balance(), clock, ctx)
}

/// `Balance`-accepting sibling of `write_collateralized`, for venues (e.g.
/// the on-chain RFQ) whose escrow lives as a `Balance`. Same checks, same
/// event. Public and permissionless-safe by construction: full collateral
/// in, `Position` + option coin out 1:1 — no premium leg, no quote bypass;
/// supply == collateral is preserved. (`write_collateralized` already
/// exposes the identical capability for `Coin` callers.)
public fun write_collateralized_balance<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    wl: &Whitelist,
    underlying: Balance<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    let amount = underlying.value();
    assert!(amount > 0, errors::zero_amount());
    let (position, call) = do_write(bucket, underlying, ctx);
    events::emit_collateralized_write(
        object::id(bucket),
        ctx.sender(),
        object::id(&position),
        amount,
        position::range_start(&position),
        position::range_end(&position),
    );
    (position, call)
}

/// Bucket mechanics shared by every write venue: escrow the underlying,
/// advance the write cursor, mint the `Position` + `Coin<Call>` pair. The
/// caller has already performed venue checks (expiry, invalidation,
/// amount > 0).
public(package) fun do_write<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    underlying: Balance<Underlying>,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    let write_amount = underlying.value();
    bucket.underlying_balance.join(underlying);

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(object::id(bucket), range_start, range_end, ctx);
    // Mint the option as a fungible coin from the bucket's own treasury.
    let call = coin::mint(&mut bucket.call_treasury, write_amount, ctx);
    (position, call)
}

/// Splits the protocol fee out of `premium` into the treasury; returns the
/// net premium balance and the fee taken. Fee = floor(premium × fee_bps /
/// 10_000), computed in u128 — matches the historical inline math exactly.
/// Public: an outside caller can only donate fees to the treasury.
public fun skim_fee<Settlement>(
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    mut premium: Balance<Settlement>,
): (Balance<Settlement>, u64) {
    let gross = premium.value();
    let fee = (((gross as u128) * (admin::fee_bps(config) as u128)) / 10000) as u64;
    if (fee > 0) {
        treasury::deposit_balance(treasury, premium.split(fee));
    };
    (premium, fee)}

public fun exercise<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    call: Coin<Call>,
    settlement_payment: Coin<Settlement>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Underlying> {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let bucket_id = object::id(bucket);

    let amount = call.value();
    assert!(amount > 0, errors::zero_amount());

    let required_settlement = apply_strike(
        amount as u128,
        bucket.strike,
        bucket.strike_scale,
    );
    assert!(settlement_payment.value() == required_settlement, errors::settlement_amount_mismatch());

    assert!(
        bucket.exercise_cursor + (amount as u128) + bucket.closed_pending
            <= bucket.total_written,
        errors::cursor_overflow(),
    );

    // Burning through the bucket's own treasury enforces, by type, that the
    // coin belongs to this bucket — no `bucket_id` field check needed.
    coin::burn(&mut bucket.call_treasury, call);

    bucket.settlement_balance.join(settlement_payment.into_balance());
    advance_cursor(bucket, amount);

    let underlying = coin::from_balance(bucket.underlying_balance.split(amount), ctx);

    events::emit_exercised(
        bucket_id,
        ctx.sender(),
        amount,
        required_settlement,
        bucket.exercise_cursor,
    );

    underlying
}

public fun redeem_position<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    position: Position,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<Underlying>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());

    let bucket_id = object::id(bucket);
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == bucket_id, errors::position_bucket_mismatch());
    // Never-physicalized spread positions are escrow-backed, not
    // pool-backed — they exit through `redeem_spread_position`.
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

    let underlying_amount = unexercised as u64;
    let settlement_amount = apply_strike(exercised, bucket.strike, bucket.strike_scale);

    let underlying = coin::from_balance(
        bucket.underlying_balance.split(underlying_amount),
        ctx,
    );
    let settlement = coin::from_balance(
        bucket.settlement_balance.split(settlement_amount),
        ctx,
    );

    events::emit_redeemed(
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

public fun burn_expired_option<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    call: Coin<Call>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let bucket_id = object::id(bucket);
    let amount = coin::burn(&mut bucket.call_treasury, call);
    events::emit_expired_option_burned(bucket_id, ctx.sender(), amount);
}

#[allow(lint(self_transfer))]
public fun cleanup_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
    bucket: Bucket<Underlying, Settlement, Call>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let Bucket {
        id,
        asset_type: _,
        settlement_type: _,
        call_type: _,
        expiry_ms: _,
        strike: _,
        strike_scale: _,
        total_written: _,
        exercise_cursor: _,
        underlying_balance,
        settlement_balance,
        call_treasury,
        invalidated: _,
        closed: _,
        closed_pending: _,
        spreads,
    } = bucket;
    // Spread escrows live as dynamic fields; every one must have been
    // unwound, closed, or redeemed before the bucket can be destroyed.
    assert!(spreads.is_empty(), errors::bucket_not_drained());
    assert!(underlying_balance.value() == 0, errors::bucket_not_drained());
    assert!(settlement_balance.value() == 0, errors::bucket_not_drained());
    underlying_balance.destroy_zero();
    settlement_balance.destroy_zero();
    // The TreasuryCap can't be dropped (no `drop`), and outstanding option
    // coins may still exist (holders who never exercised or burned). Hand
    // the cap back to the admin rather than forcing supply to zero.
    transfer::public_transfer(call_treasury, ctx.sender());
    let bucket_id = id.to_inner();
    id.delete();
    events::emit_bucket_cleaned(bucket_id);
}

public fun invalidate_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    bucket.invalidated = true;
    events::emit_bucket_invalidated(object::id(bucket), now, ctx.sender(), reason);
}

public fun revalidate_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(bucket.invalidated, errors::bucket_not_invalidated());
    bucket.invalidated = false;
    events::emit_bucket_revalidated(object::id(bucket), now, ctx.sender(), reason);
}

// ─────────────────────── exact-offset closure ───────────────────────
//
// A writer holding both a `Position` and same-bucket option coins holds
// both sides of `amount` units of the trade; netting them to zero frees
// the collateral without waiting for expiry. Mechanics: burn the coins,
// shrink the position from its range END, tombstone the closed slice so
// the FIFO cursor skips it (it holds no collateral and its coins no
// longer exist), and return the collateral. Only the unexercised part of
// a range can close (`cursor <= cut`), and closing from the end keeps
// every position a single contiguous range.
//
// Queue-consistency invariants:
//   • every `ClosedInterval` is ≥ the cursor when inserted, disjoint from
//     all position ranges (each closure shrinks its own position first);
//   • capacity = total_written − cursor − closed_pending, and the cursor
//     jumps over intervals as it meets them — so supply, pooled
//     collateral, and per-position redeem overlap all stay exact.

/// Net `call.value()` units of same-bucket option coins against the
/// caller's own `Position`, returning the freed underlying. The position
/// shrinks in place; a fully-closed position can then be destroyed with
/// `position::destroy_empty`.
public fun close_offset<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    position: &mut Position,
    call: Coin<Call>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Underlying> {
    // Post-expiry the same netting is redeem + burn_expired_option.
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let amount = call.value();
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

    coin::burn(&mut bucket.call_treasury, call);
    position::shrink_end(position, amount as u128);
    insert_closed(bucket, cut, re);
    let out = coin::from_balance(bucket.underlying_balance.split(amount), ctx);
    events::emit_offset_closed(
        object::id(bucket),
        ctx.sender(),
        object::id(position),
        false,
        amount,
        amount,
        cut,
        re,
    );
    out
}

// ─────────────────── spread collateral compression ───────────────────
//
// A long call at an equal-or-lower strike (and equal-or-later expiry) on
// the same (Underlying, Settlement) pair can back a write in place of
// underlying: the writer escrows the long coins plus EXACTLY the cash
// needed to exercise them. Physical settlement makes the cash leg
// mandatory — the pool must be able to hand exercisers real underlying —
// so "compression" means the writer never warehouses underlying, not
// that the write is collateral-free.
//
// The compressed range sits in the FIFO queue like any other write, but
// the cursor refuses to enter it until anyone cranks `unwind_spread`
// (exercise the escrowed long → its underlying joins this pool), after
// which the range and its position are indistinguishable from a physical
// write. A never-physicalized range is provably unexercised, so its
// escrow comes back untouched via `close_spread` (pre-expiry, coins
// bought back) or `redeem_spread_position` (post-expiry).

/// Write `long.value()` units backed by an escrowed long call instead of
/// underlying. `exercise_cash` must be exactly
/// `required_settlement(long_bucket, amount)`.
public fun write_spread<Underlying, Settlement, Call, LongCall>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    wl: &Whitelist,
    long_bucket: &Bucket<Underlying, Settlement, LongCall>,
    long: Coin<LongCall>,
    exercise_cash: Coin<Settlement>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    let amount = long.value();
    assert!(amount > 0, errors::zero_amount());
    // The long leg must be exercisable whenever this bucket is …
    assert!(long_bucket.expiry_ms >= bucket.expiry_ms, errors::spread_expiry_mismatch());
    // … and at an equal-or-lower strike (strike_long ≤ strike_short,
    // compared exactly across scales in u256).
    assert!(
        (long_bucket.strike as u256) * (pow10(bucket.strike_scale) as u256)
            <= (bucket.strike as u256) * (pow10(long_bucket.strike_scale) as u256),
        errors::spread_strike_too_high(),
    );
    assert!(
        exercise_cash.value() == required_settlement(long_bucket, amount),
        errors::settlement_amount_mismatch(),
    );

    let range_start = bucket.total_written;
    let range_end = range_start + (amount as u128);
    bucket.total_written = range_end;
    bucket.spreads.push_back(SpreadRange { start: range_start, end: range_end });
    df::add(
        &mut bucket.id,
        SpreadEscrowKey { start: range_start },
        SpreadEscrow<LongCall, Settlement> {
            long: long.into_balance(),
            cash: exercise_cash.into_balance(),
            long_bucket_id: object::id(long_bucket),
        },
    );

    let position = position::mint(object::id(bucket), range_start, range_end, ctx);
    let call = coin::mint(&mut bucket.call_treasury, amount, ctx);
    events::emit_spread_written(
        object::id(bucket),
        object::id(long_bucket),
        ctx.sender(),
        object::id(&position),
        amount,
        required_settlement(long_bucket, amount),
        range_start,
        range_end,
    );
    (position, call)
}

/// Permissionless crank: physicalize the compressed range starting at
/// `range_start` by exercising its escrowed long call; the resulting
/// underlying joins this bucket's pool and the range becomes an ordinary
/// write. Exercisers blocked by `spread_unwind_required` include this
/// call in their PTB — the escrow guarantees it succeeds while the long
/// bucket is live.
public fun unwind_spread<Underlying, Settlement, Call, LongCall>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    long_bucket: &mut Bucket<Underlying, Settlement, LongCall>,
    range_start: u128,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let idx = find_spread(bucket, range_start);
    let SpreadRange { start, end } = bucket.spreads.remove(idx);
    let SpreadEscrow<LongCall, Settlement> { long, cash, long_bucket_id } =
        df::remove(&mut bucket.id, SpreadEscrowKey { start });
    assert!(long_bucket_id == object::id(long_bucket), errors::spread_bucket_mismatch());
    let amount = long.value();
    let underlying = exercise(
        long_bucket,
        coin::from_balance(long, ctx),
        coin::from_balance(cash, ctx),
        clock,
        ctx,
    );
    bucket.underlying_balance.join(underlying.into_balance());
    events::emit_spread_unwound(
        object::id(bucket),
        long_bucket_id,
        ctx.sender(),
        start,
        end,
        amount,
    );
}

/// Pre-expiry spread buy-back: burn short coins equal to the full range,
/// reclaim the untouched escrow, tombstone the range. The position must
/// exactly match a live (never-physicalized) spread range — which proves
/// it is the spread's own position.
public fun close_spread<Underlying, Settlement, Call, LongCall>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    position: Position,
    call: Coin<Call>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<LongCall>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == object::id(bucket), errors::position_bucket_mismatch());
    let idx = find_spread_exact(bucket, rs, re);
    assert!((call.value() as u128) == re - rs, errors::amount_mismatch());

    coin::burn(&mut bucket.call_treasury, call);
    bucket.spreads.remove(idx);
    let SpreadEscrow<LongCall, Settlement> { long, cash, long_bucket_id: _ } =
        df::remove(&mut bucket.id, SpreadEscrowKey { start: rs });
    insert_closed(bucket, rs, re);
    events::emit_spread_closed(
        object::id(bucket),
        ctx.sender(),
        position_id,
        rs,
        re,
        (re - rs) as u64,
    );
    (coin::from_balance(long, ctx), coin::from_balance(cash, ctx))
}

/// Post-expiry exit for a never-physicalized spread position: the cursor
/// provably never entered the range, so the untouched escrow (the long
/// coins — possibly still live if the long bucket expires later — and
/// the exercise cash) returns to the holder.
public fun redeem_spread_position<Underlying, Settlement, Call, LongCall>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    position: Position,
    clock: &Clock,
    ctx: &mut TxContext,
): (Coin<LongCall>, Coin<Settlement>) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let (position_id, position_bucket_id, rs, re) = position::burn(position);
    assert!(position_bucket_id == object::id(bucket), errors::position_bucket_mismatch());
    let idx = find_spread_exact(bucket, rs, re);
    bucket.spreads.remove(idx);
    let SpreadEscrow<LongCall, Settlement> { long, cash, long_bucket_id: _ } =
        df::remove(&mut bucket.id, SpreadEscrowKey { start: rs });
    events::emit_spread_redeemed(
        object::id(bucket),
        ctx.sender(),
        position_id,
        rs,
        re,
        (re - rs) as u64,
    );
    (coin::from_balance(long, ctx), coin::from_balance(cash, ctx))
}

// ─────────────── queue-walk internals (closure + spreads) ───────────────

/// Advance the cursor by `amount` exercisable units: jump over closed
/// intervals (consuming them), abort rather than enter a spread range
/// that has not been physicalized. Capacity was already checked by the
/// caller against `closed_pending`.
fun advance_cursor<U, S, C>(bucket: &mut Bucket<U, S, C>, amount: u64) {
    let mut remaining = amount as u128;
    let mut cur = bucket.exercise_cursor;
    while (remaining > 0) {
        if (!bucket.closed.is_empty() && bucket.closed[0].start == cur) {
            let ClosedInterval { start, end } = bucket.closed.remove(0);
            bucket.closed_pending = bucket.closed_pending - (end - start);
            cur = end;
            continue
        };
        let next_closed = if (bucket.closed.is_empty()) { bucket.total_written }
        else { bucket.closed[0].start };
        let next_spread = if (bucket.spreads.is_empty()) { bucket.total_written }
        else { bucket.spreads[0].start };
        assert!(cur < next_spread, errors::spread_unwind_required());
        let limit = if (next_closed < next_spread) { next_closed } else { next_spread };
        let step = if (remaining < limit - cur) { remaining } else { limit - cur };
        cur = cur + step;
        remaining = remaining - step;
    };
    // Eagerly consume tombstones the cursor stopped flush against, so
    // `cursor == total_written` is reachable when the tail is closed.
    while (!bucket.closed.is_empty() && bucket.closed[0].start == cur) {
        let ClosedInterval { start, end } = bucket.closed.remove(0);
        bucket.closed_pending = bucket.closed_pending - (end - start);
        cur = end;
    };
    bucket.exercise_cursor = cur;
}

/// Insert a tombstone, keeping the list sorted and merging with adjacent
/// intervals. The new interval is disjoint from every existing one by
/// construction (it was carved out of a live position range).
fun insert_closed<U, S, C>(bucket: &mut Bucket<U, S, C>, start: u128, end: u128) {
    let mut i = 0;
    while (i < bucket.closed.length() && bucket.closed[i].start < start) {
        i = i + 1;
    };
    bucket.closed.insert(ClosedInterval { start, end }, i);
    if (i + 1 < bucket.closed.length() && bucket.closed[i].end == bucket.closed[i + 1].start) {
        let ClosedInterval { start: _, end: right_end } = bucket.closed.remove(i + 1);
        bucket.closed[i].end = right_end;
    };
    if (i > 0 && bucket.closed[i - 1].end == bucket.closed[i].start) {
        let ClosedInterval { start: _, end: cur_end } = bucket.closed.remove(i);
        bucket.closed[i - 1].end = cur_end;
    };
    bucket.closed_pending = bucket.closed_pending + (end - start);
}

fun overlaps_spread<U, S, C>(bucket: &Bucket<U, S, C>, start: u128, end: u128): bool {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        let s = &bucket.spreads[i];
        if (s.start < end && start < s.end) {
            return true
        };
        i = i + 1;
    };
    false
}

fun find_spread<U, S, C>(bucket: &Bucket<U, S, C>, start: u128): u64 {
    let mut i = 0;
    while (i < bucket.spreads.length()) {
        if (bucket.spreads[i].start == start) {
            return i
        };
        i = i + 1;
    };
    abort errors::spread_not_found()
}

fun find_spread_exact<U, S, C>(bucket: &Bucket<U, S, C>, start: u128, end: u128): u64 {
    let idx = find_spread(bucket, start);
    assert!(bucket.spreads[idx].end == end, errors::spread_not_found());
    idx
}

/// Strike cost for exercising `amount` option units, with the bucket's
/// round-half-up scaling (see `apply_strike`).
public fun required_settlement<U, S, C>(bucket: &Bucket<U, S, C>, amount: u64): u64 {
    apply_strike(amount as u128, bucket.strike, bucket.strike_scale)
}

public fun expiry_ms<U, S, C>(bucket: &Bucket<U, S, C>): u64 { bucket.expiry_ms }
public fun invalidated<U, S, C>(bucket: &Bucket<U, S, C>): bool { bucket.invalidated }
public fun strike<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.strike }
public fun strike_scale<U, S, C>(bucket: &Bucket<U, S, C>): u8 { bucket.strike_scale }
public fun total_written<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.total_written }
public fun exercise_cursor<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.exercise_cursor }
public fun asset_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.asset_type }
public fun settlement_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.settlement_type }
public fun call_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.call_type }

/// Units tombstoned by offset closure not yet jumped by the cursor.
public fun closed_pending<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.closed_pending }

/// Live (not yet physicalized/closed/redeemed) compressed ranges.
public fun spread_count<U, S, C>(bucket: &Bucket<U, S, C>): u64 { bucket.spreads.length() }

/// Does [start, end) overlap any live compressed range? Escrow-backed
/// ranges are not pool-backed — appraisals must value them from their
/// escrow, not from the pool's underlying.
public fun range_overlaps_spread<U, S, C>(
    bucket: &Bucket<U, S, C>,
    start: u128,
    end: u128,
): bool {
    overlaps_spread(bucket, start, end)
}

/// Escrow view for the live compressed range exactly [start, end):
/// (escrowed long units, escrowed exercise cash, long bucket id).
/// Aborts `spread_not_found` unless the exact range is live.
public fun spread_escrow_view<U, S, C, LongCall>(
    bucket: &Bucket<U, S, C>,
    start: u128,
    end: u128,
): (u64, u64, ID) {
    find_spread_exact(bucket, start, end);
    let escrow: &SpreadEscrow<LongCall, S> = df::borrow(&bucket.id, SpreadEscrowKey { start });
    (escrow.long.value(), escrow.cash.value(), escrow.long_bucket_id)
}

public fun call_supply<U, S, C>(bucket: &Bucket<U, S, C>): u64 {
    coin::total_supply(&bucket.call_treasury)
}

public fun underlying_balance<U, S, C>(bucket: &Bucket<U, S, C>): u64 {
    bucket.underlying_balance.value()
}

public fun settlement_balance<U, S, C>(bucket: &Bucket<U, S, C>): u64 {
    bucket.settlement_balance.value()
}

#[test_only]
public fun apply_strike_for_testing(amount: u128, strike: u128, strike_scale: u8): u64 {
    apply_strike(amount, strike, strike_scale)
}

#[test_only]
public fun pow10_for_testing(exp: u8): u128 {
    pow10(exp)
}
