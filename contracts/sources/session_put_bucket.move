/// Session-gated twins of the put bucket's user-facing flows (siws_session
/// integration — see `session_account` for the custody model). The mirror of
/// `session_bucket.move` for cash-secured puts: coins are sourced from and
/// settled into the user's session-linked options `Account`, and `Position`
/// objects are custodied on the account.
module options_protocol::session_put_bucket;

use std::type_name;
use sui::clock::Clock;

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::admin::ProtocolConfig;
use options_protocol::bucket::{Self, FlowKind};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position;
use options_protocol::put_bucket::{Self, PutBucket};
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::session_account;
use options_protocol::treasury::Treasury;

const SEL_EXECUTE_WRITE: vector<u8> =
    b"options_protocol::session_put_bucket::execute_write_with_session";
const SEL_EXERCISE: vector<u8> = b"options_protocol::session_put_bucket::exercise_with_session";
const SEL_REDEEM: vector<u8> =
    b"options_protocol::session_put_bucket::redeem_position_with_session";
const SEL_BURN_EXPIRED: vector<u8> =
    b"options_protocol::session_put_bucket::burn_expired_option_with_session";

public fun execute_write_selector(): vector<u8> { SEL_EXECUTE_WRITE }
public fun exercise_selector(): vector<u8> { SEL_EXERCISE }
public fun redeem_selector(): vector<u8> { SEL_REDEEM }
public fun burn_expired_selector(): vector<u8> { SEL_BURN_EXPIRED }

/// Session twin of `put_bucket::execute_write`.
public fun execute_write_with_session<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    flow: FlowKind,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_and_consume_quote(signer_account, config, &signed_quote, clock);
    execute_write_with_quote_session<Underlying, Settlement, Put>(
        bucket, config, treasury, signer_account, user_account, cap, session_account,
        flow, q, clock, ctx,
    );
}

#[test_only]
public fun execute_write_with_session_for_testing<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    flow: FlowKind,
    signed_quote: SignedQuote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let q = quote::verify_skip_sig(signer_account, config, &signed_quote, clock);
    execute_write_with_quote_session<Underlying, Settlement, Put>(
        bucket, config, treasury, signer_account, user_account, cap, session_account,
        flow, q, clock, ctx,
    );
}

fun execute_write_with_quote_session<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    flow: FlowKind,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);

    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < put_bucket::expiry_ms(bucket), errors::bucket_expired());
    assert!(!put_bucket::invalidated(bucket), errors::bucket_invalidated());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());
    let collateral_required = put_bucket::required_collateral(bucket, write_amount);

    let user_owner = account::owner(user_account);

    let (collateral, fee, position_recipient, put_token_recipient) =
        if (bucket::is_writer(&flow)) {
            // User WRITES the put: their CASH collateralizes the bucket; the
            // signer MM buys, paying premium from their Account; net premium
            // and the Position settle into the user's custody; the MM receives
            // the put coins. Collateral stays the user's claim (Position
            // redeemable back into custody), so this is cap-free (`authorize`).
            session::authorize(cap, session_account, clock, SEL_EXECUTE_WRITE, ctx.sender());
            let collateral_in = account::withdraw_internal<Settlement>(
                user_account, collateral_required, ctx,
            );
            events::emit_account_withdraw(
                object::id(user_account),
                type_name::with_defining_ids<Settlement>(),
                collateral_required,
            );

            let premium_coin = account::withdraw_internal<Settlement>(
                signer_account, gross_premium, ctx,
            );
            let (net_balance, fee) =
                bucket::skim_fee(config, treasury, premium_coin.into_balance());
            account::deposit_balance(user_account, net_balance);

            (collateral_in.into_balance(), fee, user_owner, signer_recipient)
        } else {
            // User BUYS the put: premium leaves custody into the trade; the
            // signer MM writes, providing CASH collateral from their Account
            // and receiving net premium + the Position; the put coins settle
            // into the user's custody. Cap-free (`authorize`).
            session::authorize(cap, session_account, clock, SEL_EXECUTE_WRITE, ctx.sender());
            let premium_in = account::withdraw_internal<Settlement>(
                user_account, gross_premium, ctx,
            );
            events::emit_account_withdraw(
                object::id(user_account),
                type_name::with_defining_ids<Settlement>(),
                gross_premium,
            );

            let collateral_coin = account::withdraw_internal<Settlement>(
                signer_account, collateral_required, ctx,
            );
            let (net_balance, fee) =
                bucket::skim_fee(config, treasury, premium_in.into_balance());
            account::deposit_balance(signer_account, net_balance);

            (collateral_coin.into_balance(), fee, signer_recipient, user_owner)
        };
    let net_premium = gross_premium - fee;

    let (position, put) = put_bucket::do_write(bucket, collateral, write_amount, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    if (bucket::is_writer(&flow)) {
        session_account::store_position(user_account, position);
        transfer::public_transfer(put, put_token_recipient);
    } else {
        transfer::public_transfer(position, position_recipient);
        account::deposit_balance(user_account, put.into_balance());
    };

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

/// Session twin of `put_bucket::exercise`: burns `amount` of custodied put
/// coins, delivers `amount` of custodied underlying, and credits the cash
/// proceeds back into the account. No value reaches an arbitrary recipient,
/// so this is cap-free (`authorize`).
public fun exercise_with_session<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_EXERCISE, ctx.sender());

    let put = account::withdraw_internal<Put>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Put>(),
        amount,
    );
    let underlying_delivery = account::withdraw_internal<Underlying>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Underlying>(),
        amount,
    );

    let settlement = put_bucket::exercise(bucket, put, underlying_delivery, clock, ctx);
    account::deposit_balance(user_account, settlement.into_balance());
}

/// Session twin of `put_bucket::redeem_position`: redeems a custodied
/// Position after expiry; both legs settle back into the account.
public fun redeem_position_with_session<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_REDEEM, ctx.sender());

    let position = session_account::take_position(user_account, position_id);
    let (underlying, settlement) = put_bucket::redeem_position(bucket, position, clock, ctx);
    if (underlying.value() > 0) {
        account::deposit_balance(user_account, underlying.into_balance());
    } else {
        underlying.destroy_zero();
    };
    if (settlement.value() > 0) {
        account::deposit_balance(user_account, settlement.into_balance());
    } else {
        settlement.destroy_zero();
    };
}

/// Session twin of `put_bucket::burn_expired_option`: burns the account's
/// entire custodied (now worthless) put balance for this bucket.
public fun burn_expired_option_with_session<Underlying, Settlement, Put>(
    bucket: &mut PutBucket<Underlying, Settlement, Put>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_BURN_EXPIRED, ctx.sender());

    let amount = account::balance_of<Put>(user_account);
    assert!(amount > 0, errors::zero_amount());
    let put = account::withdraw_internal<Put>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Put>(),
        amount,
    );
    put_bucket::burn_expired_option(bucket, put, clock, ctx);
}
