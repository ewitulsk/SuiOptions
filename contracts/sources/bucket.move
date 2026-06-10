module options_protocol::bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};

use options_protocol::account::{Self, Account};
use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::org::{Self, Org, OrgCap};
use options_protocol::position::{Self, Position};
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::treasury::{Self, Treasury};

/// The call option is a per-bucket fungible coin: `Coin<Call>`. `Call` is a
/// One-Time-Witness type minted from a package the options-scheduler
/// publishes per bucket set, so every bucket has its own coin currency. The
/// bucket owns the sole `TreasuryCap<Call>` — it is the only minter and
/// burner — which makes the coin's outstanding supply exactly equal to the
/// outstanding (unexercised, unburned) option amount, and makes bucket
/// isolation a type-system guarantee rather than a runtime `bucket_id` check.
public struct Bucket<phantom Underlying, phantom Settlement, phantom Call> has key {
    id: UID,
    /// Org that created (and administers) this bucket. Org fees on writes
    /// flow to this org; bucket lifecycle is gated by its OrgCap.
    org_id: ID,
    asset_type: TypeName,
    settlement_type: TypeName,
    call_type: TypeName,
    expiry_ms: u64,
    /// Strike ratio in scaled chain units. The real ratio (settlement
    /// smallest-units per underlying smallest-unit) is
    /// `strike / 10^strike_scale`. Using u128 + u8 lets a sub-cent asset
    /// paired against a same-decimal stablecoin (e.g. TDEEP/TUSDC) carry
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
    /// Freeze on new writes, toggleable pre-expiry by the bucket's OrgCap
    /// or the protocol AdminCap override. Exercises and redeems are
    /// unaffected — invalidation only blocks the write entry points.
    invalidated: bool,
}

/// Maximum supported strike_scale. 38 is the largest exponent for which
/// `pow10` still fits in u128 (`10^38 ≈ 1×10^38`, `u128::MAX ≈ 3.4×10^38`);
/// passing 39 would abort inside the loop's multiply, so we cap one below
/// that on a dedicated assert for a cleaner error.
const MAX_STRIKE_SCALE: u8 = 38;

/// WriteExecuted.flow discriminator values.
const FLOW_WRITER: u8 = 0;
const FLOW_TRADER: u8 = 1;

/// 10^exp for exp ∈ [0, MAX_STRIKE_SCALE]. Aborts if exp exceeds the cap
/// — keeps `pow10` cheap and guarantees the result fits in u128.
fun pow10(exp: u8): u128 {
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

/// Create a single bucket for the (Underlying, Settlement, Call) triple,
/// taking ownership of the option coin's `TreasuryCap`. Permissionless:
/// any OrgCap holder can create buckets; the bucket is stamped with the
/// cap's org so fees and lifecycle authority route to that org.
///
/// One bucket per call (rather than a `count` loop) because each bucket
/// needs a *distinct* `Call` coin type, and a generic function is
/// monomorphic in its type arguments per invocation. The options-scheduler
/// fans a bucket set out off-chain: it publishes one package containing N
/// One-Time-Witness coin modules, then issues N `create_bucket` calls in a
/// single PTB, one per freshly-minted `TreasuryCap`.
///
/// The cap must be fresh (zero supply) so the supply==outstanding-options
/// invariant holds from genesis.
public fun create_bucket<Underlying, Settlement, Call>(
    cap: &OrgCap,
    call_treasury: TreasuryCap<Call>,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
    ctx: &mut TxContext,
) {
    // Fail at creation rather than at the first exercise/redeem if the
    // scheduler ever hands us an out-of-range scale.
    assert!(strike_scale <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    assert!(coin::total_supply(&call_treasury) == 0, errors::treasury_cap_not_fresh());

    let org_id = org::cap_org_id(cap);
    let asset_type = type_name::with_defining_ids<Underlying>();
    let settlement_type = type_name::with_defining_ids<Settlement>();
    let call_type = type_name::with_defining_ids<Call>();
    let bucket = Bucket<Underlying, Settlement, Call> {
        id: object::new(ctx),
        org_id,
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
    };
    let bucket_id = object::id(&bucket);
    events::emit_bucket_created(
        bucket_id,
        org_id,
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        strike,
        strike_scale,
    );
    transfer::share_object(bucket);
}

/// Writer flow: the executor is the retail writer supplying underlying; the
/// signer is the trader MM (buyer) whose Account is debited the gross
/// premium. The MM's `Coin<Call>` is transferred by the contract to
/// `quote.signer_token_recipient` — the signed quote's routing guarantee —
/// while the writer's `(Position, net premium)` are returned to the PTB.
public fun execute_write_writer_flow<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Settlement>) {
    let q = quote::verify_and_consume_quote(signer_account, config, &signed_quote, clock);
    writer_flow_with_quote(
        bucket, org, config, treasury, signer_account, underlying_in, q, clock, ctx,
    )
}

/// Trader flow: the executor is the retail trader supplying the gross
/// premium; the signer is the writer MM whose Account is debited the
/// underlying and credited the net premium. The MM's `Position` is
/// transferred by the contract to `quote.signer_token_recipient`; the
/// trader's `Coin<Call>` is returned to the PTB.
public fun execute_write_trader_flow<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    premium_in: Coin<Settlement>,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Call> {
    let q = quote::verify_and_consume_quote(signer_account, config, &signed_quote, clock);
    trader_flow_with_quote(
        bucket, org, config, treasury, signer_account, premium_in, q, clock, ctx,
    )
}

#[test_only]
public fun execute_write_writer_flow_for_testing<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Settlement>) {
    let q = quote::verify_skip_sig(signer_account, config, &signed_quote, clock);
    writer_flow_with_quote(
        bucket, org, config, treasury, signer_account, underlying_in, q, clock, ctx,
    )
}

#[test_only]
public fun execute_write_trader_flow_for_testing<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    premium_in: Coin<Settlement>,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Call> {
    let q = quote::verify_skip_sig(signer_account, config, &signed_quote, clock);
    trader_flow_with_quote(
        bucket, org, config, treasury, signer_account, premium_in, q, clock, ctx,
    )
}

/// Preconditions shared by both write flows. Pause and invalidation gate
/// writes ONLY — exercise/redeem/burn/cleanup never check them.
fun check_write_preconditions<U, S, C>(
    bucket: &Bucket<U, S, C>,
    org: &Org,
    config: &ProtocolConfig,
    q: &Quote,
    clock: &Clock,
) {
    assert!(!admin::is_paused(config), errors::protocol_paused());
    assert!(quote::bucket_id(q) == object::id(bucket), errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    assert!(object::id(org) == bucket.org_id, errors::bucket_org_mismatch());
    assert!(quote::write_amount(q) > 0, errors::zero_amount());
}

/// Both fees are floored independently from gross, so rounding dust (≤ 2
/// smallest-units) stays in net_premium — never lost, and conservation
/// holds exactly: gross == org_fee + protocol_fee + net.
fun fee_split(gross: u64, org: &Org, config: &ProtocolConfig): (u64, u64) {
    let org_fee = (((gross as u128) * (org::fee_bps(org) as u128)) / 10000) as u64;
    let protocol_fee =
        (((gross as u128) * (admin::protocol_fee_bps(config) as u128)) / 10000) as u64;
    (org_fee, protocol_fee)
}

/// Skim both fees out of the premium balance. Zero fees skip the split so
/// no empty dynamic-field Balance is ever created on the Org/Treasury.
fun skim_fees<Settlement>(
    premium: &mut Balance<Settlement>,
    org: &mut Org,
    treasury: &mut Treasury,
    org_fee: u64,
    protocol_fee: u64,
) {
    if (org_fee > 0) {
        org::deposit_balance(org, premium.split(org_fee));
    };
    if (protocol_fee > 0) {
        treasury::deposit_balance(treasury, premium.split(protocol_fee));
    };
}

fun writer_flow_with_quote<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Settlement>) {
    check_write_preconditions(bucket, org, config, &q, clock);
    let bucket_id = object::id(bucket);

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(underlying_in.value() == write_amount, errors::amount_mismatch());

    let (org_fee, protocol_fee) = fee_split(gross_premium, org, config);
    let net_premium = gross_premium - org_fee - protocol_fee;

    // Signer (trader MM) pays the gross premium from their Account.
    let premium_coin = account::withdraw_internal<Settlement>(
        signer_account,
        gross_premium,
        ctx,
    );
    let mut premium_balance = premium_coin.into_balance();
    skim_fees(&mut premium_balance, org, treasury, org_fee, protocol_fee);
    let net_coin = coin::from_balance(premium_balance, ctx);

    bucket.underlying_balance.join(underlying_in.into_balance());

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(bucket_id, range_start, range_end, ctx);
    let position_id = object::id(&position);

    // The signer's side keeps its contract-enforced routing: the quote's
    // recipient gets the option coin regardless of what the executor's PTB
    // does with the returned values.
    let call = coin::mint(&mut bucket.call_treasury, write_amount, ctx);
    transfer::public_transfer(call, signer_recipient);

    events::emit_write_executed(
        bucket_id,
        bucket.org_id,
        quote::signer_account_id(&q),
        signer_recipient,
        ctx.sender(),
        position_id,
        FLOW_WRITER,
        write_amount,
        gross_premium,
        org_fee,
        protocol_fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );

    (position, net_coin)
}

fun trader_flow_with_quote<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    org: &mut Org,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    premium_in: Coin<Settlement>,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Call> {
    check_write_preconditions(bucket, org, config, &q, clock);
    let bucket_id = object::id(bucket);

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(premium_in.value() == gross_premium, errors::amount_mismatch());

    let (org_fee, protocol_fee) = fee_split(gross_premium, org, config);
    let net_premium = gross_premium - org_fee - protocol_fee;

    // Signer (writer MM) provides the underlying from their Account.
    let underlying_coin = account::withdraw_internal<Underlying>(
        signer_account,
        write_amount,
        ctx,
    );
    bucket.underlying_balance.join(underlying_coin.into_balance());

    let mut premium_balance = premium_in.into_balance();
    skim_fees(&mut premium_balance, org, treasury, org_fee, protocol_fee);
    account::deposit_balance(signer_account, premium_balance);

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(bucket_id, range_start, range_end, ctx);
    let position_id = object::id(&position);
    transfer::public_transfer(position, signer_recipient);

    let call = coin::mint(&mut bucket.call_treasury, write_amount, ctx);

    events::emit_write_executed(
        bucket_id,
        bucket.org_id,
        quote::signer_account_id(&q),
        signer_recipient,
        ctx.sender(),
        position_id,
        FLOW_TRADER,
        write_amount,
        gross_premium,
        org_fee,
        protocol_fee,
        net_premium,
        range_start,
        range_end,
        quote::nonce(&q),
    );

    call
}

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
        bucket.exercise_cursor + (amount as u128) <= bucket.total_written,
        errors::cursor_overflow(),
    );

    // Burning through the bucket's own treasury enforces, by type, that the
    // coin belongs to this bucket — no `bucket_id` field check needed.
    coin::burn(&mut bucket.call_treasury, call);

    bucket.settlement_balance.join(settlement_payment.into_balance());
    bucket.exercise_cursor = bucket.exercise_cursor + (amount as u128);

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

/// Destroy a drained, expired bucket. Gated by the bucket's OrgCap — no
/// AdminCap override, which would hand an arbitrary org's `TreasuryCap` to
/// the protocol admin. Returns the option coin's `TreasuryCap` to the PTB:
/// it can't be dropped (no `drop`), and outstanding option coins may still
/// exist (holders who never exercised or burned).
public fun cleanup_bucket<Underlying, Settlement, Call>(
    cap: &OrgCap,
    bucket: Bucket<Underlying, Settlement, Call>,
    clock: &Clock,
): TreasuryCap<Call> {
    org::assert_cap(cap, bucket.org_id);
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let Bucket {
        id,
        org_id: _,
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
    } = bucket;
    assert!(underlying_balance.value() == 0, errors::bucket_not_drained());
    assert!(settlement_balance.value() == 0, errors::bucket_not_drained());
    underlying_balance.destroy_zero();
    settlement_balance.destroy_zero();
    let bucket_id = id.to_inner();
    id.delete();
    events::emit_bucket_cleaned(bucket_id);
    call_treasury
}

public fun invalidate_bucket<Underlying, Settlement, Call>(
    cap: &OrgCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    org::assert_cap(cap, bucket.org_id);
    do_invalidate(bucket, reason, clock, ctx.sender(), false);
}

public fun revalidate_bucket<Underlying, Settlement, Call>(
    cap: &OrgCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    org::assert_cap(cap, bucket.org_id);
    do_revalidate(bucket, reason, clock, ctx.sender(), false);
}

/// Protocol-admin override: gate any org's bucket in an emergency. Note
/// `invalidated` is a single flag — an org can re-validate a bucket the
/// admin invalidated (and vice versa); last writer wins.
public fun admin_invalidate_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    do_invalidate(bucket, reason, clock, ctx.sender(), true);
}

public fun admin_revalidate_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    reason: vector<u8>,
    clock: &Clock,
    ctx: &TxContext,
) {
    do_revalidate(bucket, reason, clock, ctx.sender(), true);
}

fun do_invalidate<U, S, C>(
    bucket: &mut Bucket<U, S, C>,
    reason: vector<u8>,
    clock: &Clock,
    actor: address,
    by_admin: bool,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    bucket.invalidated = true;
    events::emit_bucket_invalidated(object::id(bucket), now, actor, by_admin, reason);
}

fun do_revalidate<U, S, C>(
    bucket: &mut Bucket<U, S, C>,
    reason: vector<u8>,
    clock: &Clock,
    actor: address,
    by_admin: bool,
) {
    let now = clock.timestamp_ms();
    assert!(now < bucket.expiry_ms, errors::bucket_expired());
    assert!(bucket.invalidated, errors::bucket_not_invalidated());
    bucket.invalidated = false;
    events::emit_bucket_revalidated(object::id(bucket), now, actor, by_admin, reason);
}

public fun org_id<U, S, C>(bucket: &Bucket<U, S, C>): ID { bucket.org_id }
public fun expiry_ms<U, S, C>(bucket: &Bucket<U, S, C>): u64 { bucket.expiry_ms }
public fun invalidated<U, S, C>(bucket: &Bucket<U, S, C>): bool { bucket.invalidated }
public fun strike<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.strike }
public fun strike_scale<U, S, C>(bucket: &Bucket<U, S, C>): u8 { bucket.strike_scale }
public fun total_written<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.total_written }
public fun exercise_cursor<U, S, C>(bucket: &Bucket<U, S, C>): u128 { bucket.exercise_cursor }
public fun asset_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.asset_type }
public fun settlement_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.settlement_type }
public fun call_type<U, S, C>(bucket: &Bucket<U, S, C>): TypeName { bucket.call_type }

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
