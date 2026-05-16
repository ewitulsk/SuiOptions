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
use options_protocol::position::{Self, PositionNFT};
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::treasury::{Self, Treasury};

public struct Bucket<phantom Underlying, phantom Settlement> has key {
    id: UID,
    asset_type: TypeName,
    settlement_type: TypeName,
    expiry_ms: u64,
    strike: u64,
    total_written: u128,
    exercise_cursor: u128,
    underlying_balance: Balance<Underlying>,
    settlement_balance: Balance<Settlement>,
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
    start_strike: u64,
    strike_interval: u64,
    count: u64,
    ctx: &mut TxContext,
) {
    assert!(count > 0, errors::count_must_be_positive());
    let asset_type = type_name::with_defining_ids<Underlying>();
    let settlement_type = type_name::with_defining_ids<Settlement>();
    let mut i: u64 = 0;
    while (i < count) {
        let strike = start_strike + i * strike_interval;
        let bucket = Bucket<Underlying, Settlement> {
            id: object::new(ctx),
            asset_type,
            settlement_type,
            expiry_ms,
            strike,
            total_written: 0,
            exercise_cursor: 0,
            underlying_balance: balance::zero<Underlying>(),
            settlement_balance: balance::zero<Settlement>(),
        };
        let bucket_id = object::id(&bucket);
        events::emit_bucket_created(bucket_id, asset_type, settlement_type, expiry_ms, strike);
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
    position_nft_recipient: address,
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
        position_nft_recipient,
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
    position_nft_recipient: address,
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
        position_nft_recipient,
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
    position_nft_recipient: address,
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
            assert!(signer_recipient == position_nft_recipient, errors::quote_recipient_mismatch());
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
    transfer::public_transfer(position, position_nft_recipient);

    let call = call_option::mint(bucket_id, write_amount, ctx);
    transfer::public_transfer(call, call_token_recipient);

    events::emit_write_executed(
        bucket_id,
        quote::signer_account_id(&q),
        signer_recipient,
        ctx.sender(),
        position_nft_recipient,
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

    let required_settlement_u128 = (amount as u128) * (bucket.strike as u128);
    let required_settlement = required_settlement_u128 as u64;
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
    position: PositionNFT,
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
    let settlement_amount = (exercised * (bucket.strike as u128)) as u64;

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
public fun strike<U, S>(bucket: &Bucket<U, S>): u64 { bucket.strike }
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
