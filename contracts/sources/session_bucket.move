/// Session-gated twins of the bucket's user-facing flows (siws_session
/// integration — see `session_account` for the custody model).
///
/// Coins are sourced from and settled into the user's session-linked options
/// `Account` (their custody vault) instead of caller-supplied coins or
/// wallet transfers, and `Position` objects are custodied on the account.
/// Each entrypoint declares its full selector for the cap's `allowed` set,
/// and value leaving custody is charged against the cap's per-type spend
/// limits via `session::authorize_spend`.
module options_protocol::session_bucket;

use std::type_name;
use sui::clock::Clock;

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::admin::ProtocolConfig;
use options_protocol::bucket::{Self, Bucket, FlowKind};
use options_protocol::errors;
use options_protocol::events;
use options_protocol::position;
use options_protocol::quote::{Self, Quote, SignedQuote};
use options_protocol::session_account;
use options_protocol::treasury::Treasury;

const SEL_EXECUTE_WRITE: vector<u8> =
    b"options_protocol::session_bucket::execute_write_with_session";
const SEL_EXERCISE: vector<u8> = b"options_protocol::session_bucket::exercise_with_session";
const SEL_REDEEM: vector<u8> =
    b"options_protocol::session_bucket::redeem_position_with_session";
const SEL_BURN_EXPIRED: vector<u8> =
    b"options_protocol::session_bucket::burn_expired_option_with_session";

public fun execute_write_selector(): vector<u8> { SEL_EXECUTE_WRITE }
public fun exercise_selector(): vector<u8> { SEL_EXERCISE }
public fun redeem_selector(): vector<u8> { SEL_REDEEM }
public fun burn_expired_selector(): vector<u8> { SEL_BURN_EXPIRED }

/// Session twin of `bucket::execute_write`. The executor side is funded from
/// the user's options `Account`; their outputs (net premium / call coins /
/// Position) settle back into it.
public fun execute_write_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    execute_write_with_quote_session<Underlying, Settlement, Call>(
        bucket, config, treasury, signer_account, user_account, cap, session_account,
        flow, q, clock, ctx,
    );
}

#[test_only]
public fun execute_write_with_session_for_testing<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    execute_write_with_quote_session<Underlying, Settlement, Call>(
        bucket, config, treasury, signer_account, user_account, cap, session_account,
        flow, q, clock, ctx,
    );
}

fun execute_write_with_quote_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    assert!(clock.timestamp_ms() < bucket::expiry_ms(bucket), errors::bucket_expired());
    assert!(!bucket::invalidated(bucket), errors::bucket_invalidated());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());

    let user_owner = account::owner(user_account);

    let (underlying, fee, position_recipient, call_token_recipient) =
        if (bucket::is_writer(&flow)) {
            // User WRITES: their underlying collateralizes the bucket; the
            // signer MM buys, paying premium from their Account; net premium
            // and the Position settle into the user's custody; the MM
            // receives the call coins. The collateral stays the user's claim
            // (Position redeemable back into this custody) and never reaches
            // an arbitrary recipient, so this is cap-free (`authorize`) — only
            // root-signed `withdraw_with_root_sig` can exit custody.
            session::authorize(cap, session_account, clock, SEL_EXECUTE_WRITE, ctx.sender());
            let underlying_in = account::withdraw_internal<Underlying>(
                user_account, write_amount, ctx,
            );
            events::emit_account_withdraw(
                object::id(user_account),
                type_name::with_defining_ids<Underlying>(),
                write_amount,
            );

            let premium_coin = account::withdraw_internal<Settlement>(
                signer_account, gross_premium, ctx,
            );
            let (net_balance, fee) =
                bucket::skim_fee(config, treasury, premium_coin.into_balance());
            account::deposit_balance(user_account, net_balance);

            (underlying_in.into_balance(), fee, user_owner, signer_recipient)
        } else {
            // User BUYS: premium leaves custody into the trade; the signer MM
            // writes, providing underlying from their Account and receiving
            // net premium + the Position; the call coins settle into the
            // user's custody. The premium buys the user a custodied call
            // position — no value reaches an arbitrary recipient — so this is
            // cap-free (`authorize`).
            session::authorize(cap, session_account, clock, SEL_EXECUTE_WRITE, ctx.sender());
            let premium_in = account::withdraw_internal<Settlement>(
                user_account, gross_premium, ctx,
            );
            events::emit_account_withdraw(
                object::id(user_account),
                type_name::with_defining_ids<Settlement>(),
                gross_premium,
            );

            let underlying_coin = account::withdraw_internal<Underlying>(
                signer_account, write_amount, ctx,
            );
            let (net_balance, fee) =
                bucket::skim_fee(config, treasury, premium_in.into_balance());
            account::deposit_balance(signer_account, net_balance);

            (underlying_coin.into_balance(), fee, signer_recipient, user_owner)
        };
    let net_premium = gross_premium - fee;

    let (position, call) = bucket::do_write(bucket, underlying, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    if (bucket::is_writer(&flow)) {
        session_account::store_position(user_account, position);
        transfer::public_transfer(call, call_token_recipient);
    } else {
        transfer::public_transfer(position, position_recipient);
        account::deposit_balance(user_account, call.into_balance());
    };

    events::emit_write_executed(
        bucket_id,
        quote::signer_account_id(&q),
        signer_recipient,
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

/// Session twin of `bucket::exercise`: burns `amount` of custodied call
/// coins, pays the strike from custodied settlement (charged against the cap
/// — the underlying proceeds return to the same custody), and credits the
/// underlying back to the account.
public fun exercise_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);

    let required_settlement = bucket::required_settlement(bucket, amount);
    // Exercise pays the strike from custody and pulls the underlying back into
    // the same custody — no value reaches an arbitrary recipient, so this is
    // cap-free (`authorize`).
    session::authorize(cap, session_account, clock, SEL_EXERCISE, ctx.sender());

    let call = account::withdraw_internal<Call>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Call>(),
        amount,
    );
    let settlement = account::withdraw_internal<Settlement>(
        user_account, required_settlement, ctx,
    );
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Settlement>(),
        required_settlement,
    );

    let underlying = bucket::exercise(bucket, call, settlement, clock, ctx);
    account::deposit_balance(user_account, underlying.into_balance());
}

/// Session twin of `bucket::redeem_position`: redeems a custodied Position
/// after expiry. No value leaves custody (both legs settle back into the
/// account), so this is `authorize`-gated only.
public fun redeem_position_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    let (underlying, settlement) = bucket::redeem_position(bucket, position, clock, ctx);
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

/// Session twin of `bucket::burn_expired_option`: burns the account's entire
/// custodied (now worthless) call balance for this bucket.
public fun burn_expired_option_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    session_account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_BURN_EXPIRED, ctx.sender());

    let amount = account::balance_of<Call>(user_account);
    assert!(amount > 0, errors::zero_amount());
    let call = account::withdraw_internal<Call>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Call>(),
        amount,
    );
    bucket::burn_expired_option(bucket, call, clock, ctx);
}
