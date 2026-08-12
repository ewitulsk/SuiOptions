/// Options-protocol adapter for the curated trading vault: post-expiry
/// redemption and conservative appraisal of custodied option `Position`s
/// (docs/trading-vault/01-contract-design.md §7 mode 1, decisions in
/// docs/vault-curator-product.md).
///
/// The on-chain RFQ/auction venue this adapter originally fronted
/// (`open_*_rfq` escrowing into `auction::create_coupled`, the desk's
/// `bid_on_auction` BidTicket custody, and their tickets' appraisal and
/// settle/reclaim cranks) is RETIRED along with the `auction` and
/// `options_rfq` packages — see `contracts/.deprecated/auction/` and
/// `contracts/.deprecated/rfq/`. The vault now writes through the
/// VaultMm quote path; what remains here is the venue-neutral tail every
/// custody path needs:
///
/// Option `Position`s are appraised at their conservative exercise-now
/// mark: exercised range at strike proceeds, unexercised range at
/// min(spot, strike) — premium upside is never marked.
module options_adapter::options_adapter;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::event;

use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, TradingVault};

// Error-code values are stable identifiers (off-chain abort-code mapping
// keys on them); retired codes are not reused.
const E_BUCKET_MISMATCH: u64 = 2;
const E_PRICE_ASSET_MISMATCH: u64 = 4;
const E_MISSING_ATTESTATION: u64 = 5;
const E_VALUE_OVERFLOW: u64 = 6;
const E_AMOUNT_OVERFLOW: u64 = 8;
const E_SPREAD_POSITION: u64 = 15;

/// Adapter witness: allowlist in `IntegrationRegistry`.
public struct OptionsAdapter has drop {}

public struct PositionRedeemed has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    underlying_out: u64,
    settlement_out: u64,
    is_put: bool,
}

/// Custody a `Position` under THIS adapter's tag. The production flow
/// that did this (the RFQ settle) is retired with the auction venue;
/// redemption/appraisal of adapter-tagged positions stays exercisable
/// in tests through this seam.
#[test_only]
public fun custody_position_for_testing(
    vault: &mut TradingVault,
    cap: &trading_vault::vault::CuratorCap,
    reg: &IntegrationRegistry,
    pos: Position,
) {
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    vault::put_position(vault, &mut s, pos);
    vault::end_session(vault, s);
}

// ═══════════════════════════ call flows ═══════════════════════════

/// Permissionless post-expiry redemption of a custodied call Position.
public fun redeem_call_position<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut Bucket<U, S, C>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = bucket::redeem_position(bucket, pos, clock, ctx);
    event::emit(PositionRedeemed {
        vault_id: vault::session_vault_id(&s),
        bucket_id: object::id(bucket),
        position_id,
        underlying_out: u_out.value(),
        settlement_out: s_out.value(),
        is_put: false,
    });
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

// ═══════════════════════════ put flows ═══════════════════════════

public fun redeem_put_position<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    bucket: &mut PutBucket<U, S, P>,
    position_id: ID,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let pos = vault::take_position<Position>(vault, &mut s, position_id);
    let (u_out, s_out) = put_bucket::redeem_position(bucket, pos, clock, ctx);
    event::emit(PositionRedeemed {
        vault_id: vault::session_vault_id(&s),
        bucket_id: object::id(bucket),
        position_id,
        underlying_out: u_out.value(),
        settlement_out: s_out.value(),
        is_put: true,
    });
    vault::put<U>(vault, &mut s, u_out.into_balance());
    vault::put<S>(vault, &mut s, s_out.into_balance());
    vault::end_session(vault, s);
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// Conservative exercise-now mark for a custodied call `Position`:
/// exercised range → strike proceeds (settlement); unexercised range →
/// min(spot, strike). Uses the bucket's own `required_settlement` math
/// so strike arithmetic is exact.
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
    // via `vault_mm::appraise_call_spread_position` on the VaultMm
    // custody path (no adapter flow can custody a spread position).
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

    // Exercised range: strike proceeds already banked in the bucket.
    let exercised_s = bucket::required_settlement(bucket, exercised);
    let mut value = value_in_deposit(vault, cfg, s_type, exercised_s, settlement_att, clock);

    // Unexercised range: worst case min(spot, strike).
    if (unexercised > 0) {
        let spot_value =
            value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let strike_s = bucket::required_settlement(bucket, unexercised);
        let strike_value = value_in_deposit(vault, cfg, s_type, strike_s, settlement_att, clock);
        value = value + spot_value.min(strike_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, position_id, value as u64);
}

/// Put twin: exercised range holds underlying (marked at spot);
/// unexercised range worst case min(spot, strike-collateral).
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
    // via `vault_mm::appraise_put_spread_position` on the VaultMm
    // custody path (no adapter flow can custody a spread position).
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
        let spot_value =
            value_in_deposit(vault, cfg, u_type, unexercised, underlying_att, clock);
        let collateral_value =
            value_in_deposit(vault, cfg, s_type, collateral_s, settlement_att, clock);
        value = value + spot_value.min(collateral_value);
    };
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, position_id, value as u64);
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
    if (asset == vault::accounting_asset(vault)) {
        return amount as u128
    };
    assert!(att.is_some(), E_MISSING_ATTESTATION);
    let a = att.extract();
    assert!(price::asset(&a) == asset, E_PRICE_ASSET_MISMATCH);
    vault::check_attestation(vault, cfg, &a, clock);
    (((amount as u256) * (price::price(&a) as u256) / (price::price_scale() as u256)) as u128)
}
