module options_protocol::bucket;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};

use siws_session::account::Account as SessionAccount;
use siws_session::session::{Self, SessionCap};

use options_protocol::account::{Self, Account};
use options_protocol::admin::{Self, AdminCap, ProtocolConfig};
use options_protocol::errors;
use options_protocol::events;
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
    /// Admin-controlled freeze on new writes. Exercises and redeems are
    /// unaffected — invalidation only blocks `execute_write`. Toggleable
    /// pre-expiry via `invalidate_bucket` / `revalidate_bucket`.
    invalidated: bool,
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

/// Create a single bucket for the (Underlying, Settlement, Call) triple,
/// taking ownership of the option coin's `TreasuryCap`.
///
/// One bucket per call (rather than the old `count` loop) because each
/// bucket needs a *distinct* `Call` coin type, and a generic function is
/// monomorphic in its type arguments per invocation. The options-scheduler
/// fans a bucket set out off-chain: it publishes one package containing N
/// One-Time-Witness coin modules, then issues N `create_bucket` calls in a
/// single PTB, one per freshly-minted `TreasuryCap`.
///
/// The cap must be fresh (zero supply) so the supply==outstanding-options
/// invariant holds from genesis.
public fun create_bucket<Underlying, Settlement, Call>(
    _: &AdminCap,
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
    };
    let bucket_id = object::id(&bucket);
    events::emit_bucket_created(
        bucket_id,
        asset_type,
        settlement_type,
        call_type,
        expiry_ms,
        strike,
        strike_scale,
    );
    transfer::share_object(bucket);
}

public fun execute_write<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    execute_write_with_quote<Underlying, Settlement, Call>(
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
public fun execute_write_for_testing<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    execute_write_with_quote<Underlying, Settlement, Call>(
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
fun execute_write_with_quote<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
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
    assert!(!bucket.invalidated, errors::bucket_invalidated());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());

    let (underlying, fee) = match (flow) {
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
            let (net_balance, fee) = skim_fee(config, treasury, premium_coin.into_balance());
            let net_coin = coin::from_balance(net_balance, ctx);
            transfer::public_transfer(net_coin, ctx.sender());

            premium_in.destroy_zero();
            (underlying_in.into_balance(), fee)
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
            let (net_balance, fee) = skim_fee(config, treasury, premium_in.into_balance());
            account::deposit_balance(signer_account, net_balance);

            underlying_in.destroy_zero();
            (underlying_coin.into_balance(), fee)
        },
    };
    let net_premium = gross_premium - fee;

    let (position, call) = do_write(bucket, underlying, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    transfer::public_transfer(position, position_recipient);
    transfer::public_transfer(call, call_token_recipient);

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
    underlying_in: Coin<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    write_collateralized_balance(bucket, underlying_in.into_balance(), clock, ctx)
}

/// `Balance`-accepting sibling of `write_collateralized`, for venues in
/// this package (e.g. the on-chain RFQ) whose escrow lives as a `Balance`.
/// Same checks, same event.
public(package) fun write_collateralized_balance<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    underlying: Balance<Underlying>,
    clock: &Clock,
    ctx: &mut TxContext,
): (Position, Coin<Call>) {
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());
    let amount = underlying.value();
    assert!(amount > 0, errors::zero_amount());
    let (position, call) = do_write(bucket, underlying, ctx);
    events::emit_collateralized_write(
        object::id(bucket),
        ctx.sender(),
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
fun do_write<Underlying, Settlement, Call>(
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
public(package) fun skim_fee<Settlement>(
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

// === Session-gated entrypoints (siws_session integration) ===
//
// Twins of the user-facing functions for session-rooted users (Solana /
// Ethereum root identity, ephemeral Sui key, sponsored gas — see
// `session-tokens/`). Coins are sourced from and settled into the user's
// session-linked options `Account` (their custody vault) instead of
// caller-supplied coins or wallet transfers, and `Position` objects are
// custodied on the account. Each entrypoint declares its full selector for
// the cap's `allowed` set, and value leaving custody is charged against the
// cap's per-type spend limits via `session::authorize_spend`.

const SEL_EXECUTE_WRITE: vector<u8> = b"options_protocol::bucket::execute_write_with_session";
const SEL_EXERCISE: vector<u8> = b"options_protocol::bucket::exercise_with_session";
const SEL_REDEEM: vector<u8> = b"options_protocol::bucket::redeem_position_with_session";
const SEL_BURN_EXPIRED: vector<u8> =
    b"options_protocol::bucket::burn_expired_option_with_session";

public fun execute_write_selector(): vector<u8> { SEL_EXECUTE_WRITE }
public fun exercise_selector(): vector<u8> { SEL_EXERCISE }
public fun redeem_selector(): vector<u8> { SEL_REDEEM }
public fun burn_expired_selector(): vector<u8> { SEL_BURN_EXPIRED }

/// Session twin of `execute_write`. The executor side is funded from the
/// user's options `Account`; their outputs (net premium / call coins /
/// Position) settle back into it.
public fun execute_write_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    signer_account: &mut Account,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &mut SessionAccount,
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
    session_account: &mut SessionAccount,
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
    session_account: &mut SessionAccount,
    flow: FlowKind,
    q: Quote,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    account::assert_session_linked(user_account, cap);

    let bucket_id = object::id(bucket);
    assert!(quote::bucket_id(&q) == bucket_id, errors::quote_bucket_mismatch());
    assert!(clock.timestamp_ms() < bucket.expiry_ms, errors::bucket_expired());
    assert!(!bucket.invalidated, errors::bucket_invalidated());

    let write_amount = quote::write_amount(&q);
    let gross_premium = quote::premium(&q);
    let signer_recipient = quote::signer_token_recipient(&q);
    assert!(write_amount > 0, errors::zero_amount());

    let user_owner = account::owner(user_account);

    let (underlying, fee, position_recipient, call_token_recipient) = match (flow) {
        FlowKind::Writer => {
            // User WRITES: their underlying collateralizes the bucket (the
            // value leaving custody — charged against the cap); the signer
            // MM buys, paying premium from their Account; net premium and
            // the Position settle into the user's custody; the MM receives
            // the call coins.
            session::authorize_spend<Underlying>(
                cap, session_account, clock, write_amount, SEL_EXECUTE_WRITE, ctx.sender(),
            );
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
            let (net_balance, fee) = skim_fee(config, treasury, premium_coin.into_balance());
            account::deposit_balance(user_account, net_balance);

            (underlying_in.into_balance(), fee, user_owner, signer_recipient)
        },
        FlowKind::Trader => {
            // User BUYS: their premium is the value leaving custody —
            // charged against the cap; the signer MM writes, providing
            // underlying from their Account and receiving net premium + the
            // Position; the call coins settle into the user's custody.
            session::authorize_spend<Settlement>(
                cap, session_account, clock, gross_premium, SEL_EXECUTE_WRITE, ctx.sender(),
            );
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
            let (net_balance, fee) = skim_fee(config, treasury, premium_in.into_balance());
            account::deposit_balance(signer_account, net_balance);

            (underlying_coin.into_balance(), fee, signer_recipient, user_owner)
        },
    };
    let net_premium = gross_premium - fee;

    let (position, call) = do_write(bucket, underlying, ctx);
    let range_start = position::range_start(&position);
    let range_end = position::range_end(&position);
    let position_id = object::id(&position);
    match (flow) {
        FlowKind::Writer => {
            account::store_position(user_account, position);
            transfer::public_transfer(call, call_token_recipient);
        },
        FlowKind::Trader => {
            transfer::public_transfer(position, position_recipient);
            account::deposit_balance(user_account, call.into_balance());
        },
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

/// Session twin of `exercise`: burns `amount` of custodied call coins, pays
/// the strike from custodied settlement (charged against the cap — the
/// underlying proceeds return to the same custody), and credits the
/// underlying back to the account.
public fun exercise_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &mut SessionAccount,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    account::assert_session_linked(user_account, cap);

    let required_settlement = apply_strike(
        amount as u128,
        bucket.strike,
        bucket.strike_scale,
    );
    session::authorize_spend<Settlement>(
        cap, session_account, clock, required_settlement, SEL_EXERCISE, ctx.sender(),
    );

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

    let underlying = exercise(bucket, call, settlement, clock, ctx);
    account::deposit_balance(user_account, underlying.into_balance());
}

/// Session twin of `redeem_position`: redeems a custodied Position after
/// expiry. No value leaves custody (both legs settle back into the account),
/// so this is `authorize`-gated only.
public fun redeem_position_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_REDEEM, ctx.sender());

    let position = account::take_position(user_account, position_id);
    let (underlying, settlement) = redeem_position(bucket, position, clock, ctx);
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

/// Session twin of `burn_expired_option`: burns the account's entire
/// custodied (now worthless) call balance for this bucket.
public fun burn_expired_option_with_session<Underlying, Settlement, Call>(
    bucket: &mut Bucket<Underlying, Settlement, Call>,
    user_account: &mut Account,
    cap: &SessionCap,
    session_account: &SessionAccount,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    account::assert_session_linked(user_account, cap);
    session::authorize(cap, session_account, clock, SEL_BURN_EXPIRED, ctx.sender());

    let amount = account::balance_of<Call>(user_account);
    assert!(amount > 0, errors::zero_amount());
    let call = account::withdraw_internal<Call>(user_account, amount, ctx);
    events::emit_account_withdraw(
        object::id(user_account),
        type_name::with_defining_ids<Call>(),
        amount,
    );
    burn_expired_option(bucket, call, clock, ctx);
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
    } = bucket;
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
