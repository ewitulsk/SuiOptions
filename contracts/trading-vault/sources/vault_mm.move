/// The vault as MARKET-MAKER COLLATERAL (design doc §7 mode 2, decisions
/// in docs/vault-curator-product.md): a first-party implementation of the
/// standardized collateral-release interface
/// (docs/audit-restructure/04-collateral-abstraction-plan.md) backed by
/// vault funds, so the curator's mm-bot can sign quotes exactly as it
/// does with `mm_collateral` — same `release<T>` shape, same PTB routing
/// via the quote's `release_package`/`release_module` — while the vault
/// underwrites the trades.
///
/// Authorization chain, in order:
/// 1. Core mints the `CollateralRequest` hot potato only after verifying
///    the signed quote (signature, expiry, nonce) — the request IS the
///    proof a quote authorized this exact debit.
/// 2. The quote's `collateral_source` must be THIS vault.
/// 3. The quote's `signer_token_recipient` must be the vault's own
///    object address — so the MM side's outputs (`Position` + net
///    premium in trader flow, `Coin<Call>` in writer flow) can only land
///    at the vault, to be swept in by the receive cranks below. Without
///    this check a curator could sign quotes routing the trade's
///    proceeds to themselves.
/// 4. The curator must have opted in (`mm_release_enabled`, off by
///    default) — the per-vault kill switch, since the standardized
///    3-argument `release` signature leaves no room for a registry.
///
/// Sweeps, redeems and appraisals run under the `VaultMm` witness, which
/// must be allowlisted in the `IntegrationRegistry` like any adapter.
/// NAV note: between a release and its sweep the value is in flight
/// (transferred objects at the vault address are invisible to
/// appraisal), so NAV dips transiently by at most the outstanding quote
/// sizes; keepers should sweep promptly.
module trading_vault::vault_mm;

use std::type_name::{Self, TypeName};
use sui::balance::Balance;
use sui::clock::Clock;
use sui::coin::Coin;
use sui::event;
use sui::transfer::Receiving;

use options_core::bucket::{Self, Bucket};
use options_core::collateral::{Self, CollateralRequest};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};

use trading_vault::events;
use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, CuratorCap, TradingVault};

const E_WRONG_SOURCE: u64 = 1;
const E_RECIPIENT_NOT_VAULT: u64 = 2;
const E_MM_DISABLED: u64 = 3;
const E_BUCKET_MISMATCH: u64 = 4;
const E_PRICE_ASSET_MISMATCH: u64 = 5;
const E_MISSING_ATTESTATION: u64 = 6;
const E_VALUE_OVERFLOW: u64 = 7;
const E_AMOUNT_OVERFLOW: u64 = 8;
const E_SPREAD_POSITION: u64 = 9;
const E_LONG_BUCKET_MISMATCH: u64 = 10;

/// Adapter witness for sweeps/cranks/appraisal; allowlist to enable.
public struct VaultMm has drop {}

public struct CollateralReleased has copy, drop {
    vault_id: ID,
    asset_type: TypeName,
    amount: u64,
    bucket_id: ID,
    quote_nonce: u64,
    is_writer_flow: bool,
}

// ═══════════════════ the standardized release surface ═══════════════════

/// `{release_package}::vault_mm::release` — the exact 3-argument shape
/// core's execute flows and the off-chain PTB builders expect.
public fun release<T>(
    vault: &mut TradingVault,
    request: &CollateralRequest<T>,
    _ctx: &mut TxContext,
): Balance<T> {
    assert!(vault::mm_release_enabled(vault), E_MM_DISABLED);
    let vault_id = object::id(vault);
    assert!(collateral::source(request) == vault_id, E_WRONG_SOURCE);
    assert!(
        collateral::signer_token_recipient(request) == vault_id.to_address(),
        E_RECIPIENT_NOT_VAULT,
    );
    let amount = collateral::amount(request);
    let funds = vault::release_for_mm<T>(vault, amount);
    event::emit(CollateralReleased {
        vault_id,
        asset_type: type_name::with_defining_ids<T>(),
        amount,
        bucket_id: collateral::bucket_id(request),
        quote_nonce: collateral::quote_nonce(request),
        is_writer_flow: collateral::is_writer_flow(request),
    });
    funds
}

// ═══════════════════════════ sweeps (cranks) ═══════════════════════════

/// Sweep a `Position` minted to the vault's address (trader flow).
public fun receive_mm_position(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    receiving: Receiving<Position>,
) {
    vault::receive_position<Position, VaultMm>(vault, reg, VaultMm {}, receiving);
}

/// Sweep an option coin (writer flow: the vault bought the option) into
/// custody AS A POSITION — never into free balances, where an unpriceable
/// asset type would wedge every appraisal.
public fun receive_mm_option_coin<C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    receiving: Receiving<Coin<C>>,
) {
    vault::receive_position<Coin<C>, VaultMm>(vault, reg, VaultMm {}, receiving);
}

/// Sweep a plain coin transferred to the vault's address (trader-flow
/// net premium) into free balances.
public fun receive_mm_coin<T>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    receiving: Receiving<Coin<T>>,
) {
    vault::receive_coin<T, VaultMm>(vault, reg, VaultMm {}, receiving);
}

// ═══════════════════════ redeem / exercise ═══════════════════════

/// Permissionless post-expiry redemption of a VaultMm-tagged call
/// Position.
public fun redeem_call_position<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut Bucket<U, S, C>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, VaultMm {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = bucket::redeem_position(bucket, pos, clock, ctx);
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

/// Put twin.
public fun redeem_put_position<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut PutBucket<U, S, P>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, VaultMm {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = put_bucket::redeem_position(bucket, pos, clock, ctx);
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

/// Curator exercises `amount` of a custodied call coin, paying
/// `required_settlement` from vault free balances; the freed underlying
/// returns to free balances and the remainder (if any) stays in custody
/// under the same position id (net-zero position count, so
/// `max_positions` can never bind here).
///
/// Cap-gated by design — NOT exposed as a crank: exercising is
/// discretionary (it trades time value for intrinsic and spends vault
/// settlement), so a permissionless caller could grief the strategy by
/// exercising OTM or too early. Post-expiry cleanup has its own crank
/// (`redeem_*_position`); pre-expiry exercise stays a curator call.
public fun exercise_call_coin<U, S, C>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &mut Bucket<U, S, C>,
    coin_position_id: ID,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, VaultMm {});
    let mut call: Coin<C> = vault::take_position<Coin<C>>(vault, &mut s, coin_position_id);
    let payment_amount = bucket::required_settlement(bucket, amount);
    let payment = sui::coin::from_balance(
        vault::take<S>(vault, &mut s, payment_amount),
        ctx,
    );
    let slice = call.split(amount, ctx);
    let u_out = bucket::exercise(bucket, slice, payment, clock, ctx);
    vault::put<U>(vault, &mut s, u_out.into_balance());
    if (call.value() > 0) {
        vault::put_position(vault, &mut s, call);
    } else {
        call.destroy_zero();
    };
    events::emit_mm_coin_exercised(
        object::id(vault),
        object::id(bucket),
        coin_position_id,
        false,
        amount,
        payment_amount,
    );
    vault::end_session(vault, s);
}

/// Put twin: deliver `amount` underlying from vault free balances
/// against the custodied put coin; the strike payout returns as free
/// settlement. Cap-gated for the same reason as `exercise_call_coin`.
public fun exercise_put_coin<U, S, P>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &mut PutBucket<U, S, P>,
    coin_position_id: ID,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, VaultMm {});
    let mut put: Coin<P> = vault::take_position<Coin<P>>(vault, &mut s, coin_position_id);
    let delivery = sui::coin::from_balance(
        vault::take<U>(vault, &mut s, amount),
        ctx,
    );
    let slice = put.split(amount, ctx);
    let s_out = put_bucket::exercise(bucket, slice, delivery, clock, ctx);
    let payout = s_out.value();
    vault::put<S>(vault, &mut s, s_out.into_balance());
    if (put.value() > 0) {
        vault::put_position(vault, &mut s, put);
    } else {
        put.destroy_zero();
    };
    events::emit_mm_coin_exercised(
        object::id(vault),
        object::id(bucket),
        coin_position_id,
        true,
        amount,
        payout,
    );
    vault::end_session(vault, s);
}

// ═══════════════════════ offset closure / release ═══════════════════════

/// Net `amount` of a custodied same-bucket call coin against a custodied
/// written `Position` (both VaultMm-tagged) via `bucket::close_offset`,
/// returning the freed underlying to free balances. The coin remainder
/// re-stores under its position id; the shrunk `Position` re-stores
/// unless fully closed, in which case it is destroyed
/// (`position::destroy_empty`). Curator-only: closing early is as
/// discretionary as exercising.
public fun close_offset_position<U, S, C>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &mut Bucket<U, S, C>,
    position_id: ID,
    coin_position_id: ID,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, VaultMm {});
    let mut pos = vault::take_position<Position>(vault, &mut s, position_id);
    let mut call: Coin<C> = vault::take_position<Coin<C>>(vault, &mut s, coin_position_id);
    let slice = call.split(amount, ctx);
    let freed = bucket::close_offset(bucket, &mut pos, slice, clock, ctx);
    let collateral_returned = freed.value();
    vault::put<U>(vault, &mut s, freed.into_balance());
    if (call.value() > 0) {
        vault::put_position(vault, &mut s, call);
    } else {
        call.destroy_zero();
    };
    let position_closed = position::amount(&pos) == 0;
    if (position_closed) {
        position::destroy_empty(pos);
    } else {
        vault::put_position(vault, &mut s, pos);
    };
    events::emit_mm_offset_closed(
        object::id(vault),
        object::id(bucket),
        position_id,
        false,
        amount,
        collateral_returned,
        position_closed,
    );
    vault::end_session(vault, s);
}

/// Put twin: nets custodied put coins against the custodied written put
/// `Position`, returning the freed cash collateral
/// (`floor(amount × strike)`) to free balances.
public fun close_offset_put_position<U, S, P>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &mut PutBucket<U, S, P>,
    position_id: ID,
    coin_position_id: ID,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_session(vault, cap, reg, VaultMm {});
    let mut pos = vault::take_position<Position>(vault, &mut s, position_id);
    let mut put: Coin<P> = vault::take_position<Coin<P>>(vault, &mut s, coin_position_id);
    let slice = put.split(amount, ctx);
    let freed = put_bucket::close_offset(bucket, &mut pos, slice, clock, ctx);
    let collateral_returned = freed.value();
    vault::put<S>(vault, &mut s, freed.into_balance());
    if (put.value() > 0) {
        vault::put_position(vault, &mut s, put);
    } else {
        put.destroy_zero();
    };
    let position_closed = position::amount(&pos) == 0;
    if (position_closed) {
        position::destroy_empty(pos);
    } else {
        vault::put_position(vault, &mut s, pos);
    };
    events::emit_mm_offset_closed(
        object::id(vault),
        object::id(bucket),
        position_id,
        true,
        amount,
        collateral_returned,
        position_closed,
    );
    vault::end_session(vault, s);
}

/// Move a VaultMm-custodied coin (option coins from the writer-flow
/// sweep) out of position custody into the vault's FREE balances.
///
/// Coins were historically kept AS POSITIONS because an unpriceable
/// free-balance asset type wedges every appraisal. Since SO-297,
/// `options_oracle` attests option-coin intrinsics, so the coin is
/// appraisable as a free balance (`appraise_balance` with an
/// attestation for `T`) and freeing it is safe. This is the resale
/// on-ramp: compose in one PTB with the deepbook adapter's taker swap
/// (`release_coin_to_balances<CALL>` →
/// `taker_swap_base_for_quote<CALL, USDC>`). The inverse is not needed:
/// the receive path (`receive_mm_option_coin`) already stores incoming
/// coins as positions.
public fun release_coin_to_balances<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    coin_position_id: ID,
) {
    let mut s = vault::begin_session(vault, cap, reg, VaultMm {});
    let c: Coin<T> = vault::take_position<Coin<T>>(vault, &mut s, coin_position_id);
    events::emit_mm_coin_released(
        object::id(vault),
        coin_position_id,
        type_name::with_defining_ids<T>(),
        c.value(),
    );
    vault::put<T>(vault, &mut s, c.into_balance());
    vault::end_session(vault, s);
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// Conservative marks, mirroring `options_adapter` for VaultMm-tagged
/// custody: written positions at exercise-now, held option coins at
/// intrinsic.
public fun appraise_call_position<U, S, C>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &Bucket<U, S, C>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    // A compressed range is escrow-backed, not pool-backed — it marks
    // via `appraise_call_spread_position`, never here.
    assert!(
        !bucket::range_overlaps_spread(
            bucket,
            position::range_start(pos),
            position::range_end(pos),
        ),
        E_SPREAD_POSITION,
    );
    let (exercised, unexercised) = split_ranges(
        position::range_start(pos),
        position::range_end(pos),
        bucket::exercise_cursor(bucket),
    );
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let exercised_s = bucket::required_settlement(bucket, exercised);
    let mut value = value_in_deposit(vault, cfg, s_type, exercised_s, settlement_att, clock);
    if (unexercised > 0) {
        let spot_value = value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let strike_s = bucket::required_settlement(bucket, unexercised);
        let strike_value = value_in_deposit(vault, cfg, s_type, strike_s, settlement_att, clock);
        value = value + spot_value.min(strike_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, position_id, value as u64);
}

/// Conservative mark for a VaultMm-custodied SPREAD position — a
/// compressed range from `bucket::write_spread`, not yet physicalized,
/// backed by its escrow (long coins + exact long-exercise cash), not
/// pool underlying. Mark = escrow cash + (long intrinsic − short
/// intrinsic): equal to the physical min(spot, strike) mark whenever
/// spot ≥ strike_long, floored at the escrowed cash below it. Aborts
/// unless the position's exact range is a live spread escrowed against
/// `long_bucket`. Mirrors `options_adapter`.
public fun appraise_call_spread_position<U, S, C, LongCall>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &Bucket<U, S, C>,
    long_bucket: &Bucket<U, S, LongCall>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    let (long_units, cash, long_bucket_id) = bucket::spread_escrow_view<U, S, C, LongCall>(
        bucket,
        position::range_start(pos),
        position::range_end(pos),
    );
    assert!(long_bucket_id == object::id(long_bucket), E_LONG_BUCKET_MISMATCH);

    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let spot = value_in_deposit(vault, cfg, u_type, long_units, underlying_att, clock);
    let long_strike = value_in_deposit(
        vault,
        cfg,
        s_type,
        bucket::required_settlement(long_bucket, long_units),
        settlement_att,
        clock,
    );
    let short_strike = value_in_deposit(
        vault,
        cfg,
        s_type,
        bucket::required_settlement(bucket, long_units),
        settlement_att,
        clock,
    );
    let cash_value = value_in_deposit(vault, cfg, s_type, cash, settlement_att, clock);
    // (spot − K_long)⁺ − (spot − K_short)⁺ rewritten subtraction-safe:
    // min(spot, K_short) − min(spot, K_long), with K_long ≤ K_short
    // guaranteed by `write_spread`.
    let value = cash_value + spot.min(short_strike) - spot.min(long_strike);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, position_id, value as u64);
}

public fun appraise_put_position<U, S, P>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &PutBucket<U, S, P>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    // A compressed range is escrow-backed, not pool-backed — it marks
    // via `appraise_put_spread_position`, never here.
    assert!(
        !put_bucket::range_overlaps_spread(
            bucket,
            position::range_start(pos),
            position::range_end(pos),
        ),
        E_SPREAD_POSITION,
    );
    let (exercised, unexercised) = split_ranges(
        position::range_start(pos),
        position::range_end(pos),
        put_bucket::exercise_cursor(bucket),
    );
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let mut value = value_in_deposit(vault, cfg, u_type, exercised, underlying_att, clock);
    if (unexercised > 0) {
        let collateral_s = put_bucket::required_collateral(bucket, unexercised);
        let spot_value = value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let collateral_value =
            value_in_deposit(vault, cfg, s_type, collateral_s, settlement_att, clock);
        value = value + spot_value.min(collateral_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, position_id, value as u64);
}

/// Conservative mark for a VaultMm-custodied put SPREAD position — a
/// compressed range from `put_bucket::write_spread`, backed by its
/// escrow (remaining long puts + gap cash incl. accrued assignment
/// surplus). Mark = escrow cash + (long-put intrinsic − short-put
/// intrinsic) on the UNASSIGNED remainder, floored at the escrow cash;
/// assigned slices already settled at exercise time and contribute
/// nothing beyond the surplus the escrow retained. Aborts unless the
/// position's exact range is a live spread escrowed against
/// `long_bucket`.
public fun appraise_put_spread_position<U, S, P, LongPut>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &PutBucket<U, S, P>,
    long_bucket: &PutBucket<U, S, LongPut>,
    position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let pos: &Position = vault::borrow_position(vault, position_id);
    assert!(position::bucket_id(pos) == object::id(bucket), E_BUCKET_MISMATCH);
    let (long_units, cash, long_bucket_id) = put_bucket::spread_escrow_view<U, S, P, LongPut>(
        bucket,
        position::range_start(pos),
        position::range_end(pos),
    );
    assert!(long_bucket_id == object::id(long_bucket), E_LONG_BUCKET_MISMATCH);

    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let cash_value = value_in_deposit(vault, cfg, s_type, cash, settlement_att, clock);
    if (long_units == 0) {
        assert!(cash_value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
        vault::record_position_value(vault, appraisal, VaultMm {}, position_id, cash_value as u64);
        return
    };
    let spot = value_in_deposit(vault, cfg, u_type, long_units, underlying_att, clock);
    let long_strike = value_in_deposit(
        vault,
        cfg,
        s_type,
        put_bucket::exercise_payout(long_bucket, long_units),
        settlement_att,
        clock,
    );
    let short_strike = value_in_deposit(
        vault,
        cfg,
        s_type,
        put_bucket::exercise_payout(bucket, long_units),
        settlement_att,
        clock,
    );
    // (K_long − S)⁺ − (K_short − S)⁺ rewritten subtraction-safe, with a
    // zero floor absorbing rounding dust when K_short > K_long exceeds
    // the escrowed gap by a unit.
    let lhs = cash_value + long_strike + spot.min(short_strike);
    let rhs = short_strike + spot.min(long_strike);
    let value = if (lhs > rhs) { lhs - rhs } else { 0 };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, position_id, value as u64);
}

/// A held call coin marks at intrinsic: max(spot − strike, 0). Expired
/// coins mark at zero (exercise is pre-expiry only) with no attestations
/// needed.
public fun appraise_call_coin<U, S, C>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &Bucket<U, S, C>,
    coin_position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let call: &Coin<C> = vault::borrow_position(vault, coin_position_id);
    if (clock.timestamp_ms() >= bucket::expiry_ms(bucket)) {
        vault::record_position_value(vault, appraisal, VaultMm {}, coin_position_id, 0);
        return
    };
    let amount = call.value();
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let spot_value = value_in_deposit(vault, cfg, u_type, amount, underlying_att, clock);
    let strike_s = bucket::required_settlement(bucket, amount);
    let strike_value = value_in_deposit(vault, cfg, s_type, strike_s, settlement_att, clock);
    let value = if (spot_value > strike_value) { spot_value - strike_value } else { 0 };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, coin_position_id, value as u64);
}

/// Put twin: a held put coin marks at max(strike payout − spot delivery
/// cost, 0) — exercising delivers underlying against the floor-rounded
/// strike payout. Expired puts mark at zero.
public fun appraise_put_coin<U, S, P>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    bucket: &PutBucket<U, S, P>,
    coin_position_id: ID,
    underlying_att: Option<PriceAttestation>,
    settlement_att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let put: &Coin<P> = vault::borrow_position(vault, coin_position_id);
    if (clock.timestamp_ms() >= put_bucket::expiry_ms(bucket)) {
        vault::record_position_value(vault, appraisal, VaultMm {}, coin_position_id, 0);
        return
    };
    let amount = put.value();
    let u_type = type_name::with_defining_ids<U>();
    let s_type = type_name::with_defining_ids<S>();
    let payout_s = put_bucket::exercise_payout(bucket, amount);
    let payout_value = value_in_deposit(vault, cfg, s_type, payout_s, settlement_att, clock);
    let spot_value = value_in_deposit(vault, cfg, u_type, amount, underlying_att, clock);
    let value = if (payout_value > spot_value) { payout_value - spot_value } else { 0 };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, VaultMm {}, coin_position_id, value as u64);
}

// ═══════════════════════════════ internals ═══════════════════════════════

fun split_ranges(range_start: u128, range_end: u128, cursor: u128): (u64, u64) {
    let amount = range_end - range_start;
    let exercised = if (cursor <= range_start) {
        0
    } else if (cursor >= range_end) {
        amount
    } else {
        cursor - range_start
    };
    let unexercised = amount - exercised;
    assert!(
        exercised <= (std::u64::max_value!() as u128)
            && unexercised <= (std::u64::max_value!() as u128),
        E_AMOUNT_OVERFLOW,
    );
    ((exercised as u64), (unexercised as u64))
}

fun value_in_deposit(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    asset: TypeName,
    amount: u64,
    mut att: Option<PriceAttestation>,
    clock: &Clock,
): u128 {
    if (amount == 0) {
        return 0
    };
    if (asset == vault::deposit_asset(vault)) {
        return amount as u128
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == asset, E_PRICE_ASSET_MISMATCH);
    vault::check_attestation(vault, cfg, &a, clock);
    (((amount as u256) * (price::price(&a) as u256) / (price::price_scale() as u256)) as u128)
}
