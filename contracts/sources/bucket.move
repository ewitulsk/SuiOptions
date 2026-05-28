module options_protocol::bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};

use options_protocol::account::{Self, Account};
use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::call_option::{Self, CallOption};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position::{Self, Position};
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::treasury::{Self, Treasury};

public struct Bucket<phantom Underlying, phantom Settlement> has key {
    id: UID,
    asset_type: TypeName,
    settlement_type: TypeName,
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
}

/// Maximum supported strike_scale. 38 is the largest exponent for which
/// `pow10` still fits in u128 (`10^38 ≈ 1×10^38`, `u128::MAX ≈ 3.4×10^38`);
/// passing 39 would abort inside the loop's multiply, so we cap one below
/// that on a dedicated assert for a cleaner error.
const MAX_STRIKE_SCALE: u8 = 38;

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

public enum FlowKind has copy, drop, store {
    Writer,
    Trader,
}

public fun writer_flow(): FlowKind { FlowKind::Writer }

public fun trader_flow(): FlowKind { FlowKind::Trader }

public fun new_call_option<Underlying, Settlement>(
    _: &AdminCap,
    expiry_ms: u64,
    start_strike: u128,
    strike_interval: u128,
    count: u64,
    strike_scale: u8,
    ctx: &mut TxContext,
) {
    assert!(count > 0, errors::count_must_be_positive());
    // Fail at creation rather than at the first exercise/redeem if the
    // scheduler ever hands us an out-of-range scale.
    assert!(strike_scale <= MAX_STRIKE_SCALE, errors::strike_scale_too_large());
    let asset_type = type_name::with_defining_ids<Underlying>();
    let settlement_type = type_name::with_defining_ids<Settlement>();
    let mut i: u64 = 0;
    while (i < count) {
        let strike = start_strike + (i as u128) * strike_interval;
        let bucket = Bucket<Underlying, Settlement> {
            id: object::new(ctx),
            asset_type,
            settlement_type,
            expiry_ms,
            strike,
            strike_scale,
            total_written: 0,
            exercise_cursor: 0,
            underlying_balance: balance::zero<Underlying>(),
            settlement_balance: balance::zero<Settlement>(),
        };
        let bucket_id = object::id(&bucket);
        events::emit_bucket_created(
            bucket_id,
            asset_type,
            settlement_type,
            expiry_ms,
            strike,
            strike_scale,
        );
        transfer::share_object(bucket);
        i = i + 1;
    };
}

public fun execute_write<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    call_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_and_consume_quote(signer_account, config, &signed_quote, clock);
    execute_write_with_quote<Underlying, Settlement>(
        bucket,
        config,
        treasury,
        signer_account,
        underlying_in,
        premium_in,
        flow,
        position_recipient,
        call_token_recipient,
        q,
        clock,
        ctx,
    );
}

#[test_only]
public fun execute_write_for_testing<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    call_token_recipient: address,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_skip_sig(signer_account, config, &signed_quote, clock);
    execute_write_with_quote<Underlying, Settlement>(
        bucket,
        config,
        treasury,
        signer_account,
        underlying_in,
        premium_in,
        flow,
        position_recipient,
        call_token_recipient,
        q,
        clock,
        ctx,
    );
}

#[allow(lint(self_transfer))]
fun execute_write_with_quote<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    underlying_in: Coin<Underlying>,
    premium_in: Coin<Settlement>,
    flow: FlowKind,
    position_recipient: address,
    call_token_recipient: address,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());

    let fee = (((gross_premium as u128) * (admin::fee_bps(config) as u128)) / 10000) as u64;
    let net_premium = gross_premium - fee;

    match (flow) {
        FlowKind::Writer => {
            // Signer is the trader MM (the buyer of the option).
            // Signer-supplied side: premium (Settlement) debited from their Account.
            // Executor-supplied side: underlying matching write_amount.
            assert!(signer_recipient == call_token_recipient, errors::quote_recipient_mismatch());
            assert!(premium_in.value() == 0, errors::amount_mismatch());
            assert!(underlying_in.value() == write_amount, errors::amount_mismatch());

            let premium_coin = account::withdraw_internal<Settlement>(
                signer_account,
                gross_premium,
                ctx,
            );
            let mut premium_balance = premium_coin.into_balance();
            if (fee > 0) {
                let fee_balance = premium_balance.split(fee);
                treasury::deposit_balance(treasury, fee_balance);
            };
            let net_coin = coin::from_balance(premium_balance, ctx);
            transfer::public_transfer(net_coin, ctx.sender());

            bucket.underlying_balance.join(underlying_in.into_balance());
            premium_in.destroy_zero();
        },
        FlowKind::Trader => {
            // Signer is the writer MM (the seller of the option).
            // Signer-supplied side: underlying debited from their Account.
            // Executor-supplied side: premium matching gross_premium.
            assert!(signer_recipient == position_recipient, errors::quote_recipient_mismatch());
            assert!(underlying_in.value() == 0, errors::amount_mismatch());
            assert!(premium_in.value() == gross_premium, errors::amount_mismatch());

            let underlying_coin = account::withdraw_internal<Underlying>(
                signer_account,
                write_amount,
                ctx,
            );
            bucket.underlying_balance.join(underlying_coin.into_balance());

            let mut premium_balance = premium_in.into_balance();
            if (fee > 0) {
                let fee_balance = premium_balance.split(fee);
                treasury::deposit_balance(treasury, fee_balance);
            };
            account::deposit_balance(signer_account, premium_balance);

            underlying_in.destroy_zero();
        },
    };

    let range_start = bucket.total_written;
    let range_end = range_start + (write_amount as u128);
    bucket.total_written = range_end;

    let position = position::mint(bucket_id, range_start, range_end, ctx);
    transfer::public_transfer(position, position_recipient);

    let call = call_option::mint(bucket_id, write_amount, ctx);
    transfer::public_transfer(call, call_token_recipient);

    events::emit_write_executed(
        bucket_id,
        quote::signer_account_id(&q),
        signer_recipient,
        ctx.sender(),
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

public fun exercise<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    call: CallOption,
    settlement_payment: Coin<Settlement>,
    clock: &Clock,
    ctx: &mut TxContext,
): Coin<Underlying> {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    let bucket_id = object::id(bucket);
    assert!(call_option::bucket_id(&call) == bucket_id, errors::call_option_bucket_mismatch());

    let amount = call_option::amount(&call);
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

    call_option::burn(call);

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

public fun redeem_position<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
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

public fun burn_expired_option<Underlying, Settlement>(
    bucket: &mut Bucket<Underlying, Settlement>,
    call: CallOption,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let bucket_id = object::id(bucket);
    assert!(call_option::bucket_id(&call) == bucket_id, errors::call_option_bucket_mismatch());
    let amount = call_option::burn(call);
    events::emit_expired_option_burned(bucket_id, ctx.sender(), amount);
}

public fun cleanup_bucket<Underlying, Settlement>(
    _: &AdminCap,
    bucket: Bucket<Underlying, Settlement>,
    clock: &Clock,
) {
    assert!(clock.timestamp_ms() >= bucket.expiry_ms, errors::bucket_not_expired());
    let Bucket {
        id,
        asset_type: _,
        settlement_type: _,
        expiry_ms: _,
        strike: _,
        strike_scale: _,
        total_written: _,
        exercise_cursor: _,
        underlying_balance,
        settlement_balance,
    } = bucket;
    assert!(underlying_balance.value() == 0, errors::bucket_not_drained());
    assert!(settlement_balance.value() == 0, errors::bucket_not_drained());
    underlying_balance.destroy_zero();
    settlement_balance.destroy_zero();
    let bucket_id = id.to_inner();
    id.delete();
    events::emit_bucket_cleaned(bucket_id);
}

public fun expiry_ms<U, S>(bucket: &Bucket<U, S>): u64 { bucket.expiry_ms }
public fun strike<U, S>(bucket: &Bucket<U, S>): u128 { bucket.strike }
public fun strike_scale<U, S>(bucket: &Bucket<U, S>): u8 { bucket.strike_scale }
public fun total_written<U, S>(bucket: &Bucket<U, S>): u128 { bucket.total_written }
public fun exercise_cursor<U, S>(bucket: &Bucket<U, S>): u128 { bucket.exercise_cursor }
public fun asset_type<U, S>(bucket: &Bucket<U, S>): TypeName { bucket.asset_type }
public fun settlement_type<U, S>(bucket: &Bucket<U, S>): TypeName { bucket.settlement_type }

public fun underlying_balance<U, S>(bucket: &Bucket<U, S>): u64 {
    bucket.underlying_balance.value()
}

public fun settlement_balance<U, S>(bucket: &Bucket<U, S>): u64 {
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
