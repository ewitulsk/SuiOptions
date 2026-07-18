/// Options-protocol adapter for the curated trading vault: the vault as
/// covered-call (and cash-secured-put) WRITER via the on-chain RFQ
/// (docs/trading-vault/01-contract-design.md §7 mode 1, decisions in
/// docs/vault-curator-product.md).
///
/// Mirrors `options_vault`'s inlined pattern rather than the
/// `options_rfq` package: the curator escrows vault funds straight into
/// a generic `auction::create_coupled` with THIS module's witness as
/// settle authority and the VAULT as origin; market makers bid premium
/// on the shared auction; the permissionless settle crank writes into
/// the bucket and absorbs the `Position` + net premium back into the
/// vault. (`options_rfq` routes outputs to plain addresses, which is the
/// wrong shape for a shared-object vault.)
///
/// While an auction is live the escrow sits outside the vault, so each
/// open RFQ is represented by an `RfqTicket` position appraised at
/// escrow cost. Option `Position`s are appraised at their conservative
/// exercise-now mark: exercised range at strike proceeds, unexercised
/// range at min(spot, strike) — premium upside is never marked.
module options_adapter::options_adapter;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::coin;
use sui::event;

use auction::auction::{Self as auctions, Auction};
use options_core::admin::ProtocolConfig;
use options_core::bucket::{Self, Bucket};
use options_core::position::{Self, Position};
use options_core::put_bucket::{Self, PutBucket};
use options_core::treasury::Treasury;

use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{IntegrationRegistry, VaultProtocolConfig};
use trading_vault::vault::{Self, Appraisal, CuratorCap, TradingVault};

const E_WRONG_TICKET: u64 = 1;
const E_BUCKET_MISMATCH: u64 = 2;
const E_BUCKET_LIVE: u64 = 3;
const E_PRICE_ASSET_MISMATCH: u64 = 4;
const E_MISSING_ATTESTATION: u64 = 5;
const E_VALUE_OVERFLOW: u64 = 6;
const E_ESCROW_TYPE_MISMATCH: u64 = 7;
const E_AMOUNT_OVERFLOW: u64 = 8;

/// Adapter witness: allowlist in `IntegrationRegistry`; also the
/// auctions' settle authority.
public struct OptionsAdapter has drop {}

/// Open-RFQ marker held as a vault position while the escrow lives in
/// the shared `Auction`. Appraised at escrow cost.
public struct RfqTicket has key, store {
    id: UID,
    vault_id: ID,
    auction_id: ID,
    bucket_id: ID,
    /// Option contracts being written.
    write_amount: u64,
    /// Escrow paid in: underlying (calls) or settlement collateral (puts).
    escrow_amount: u64,
    escrow_type: TypeName,
    is_put: bool,
}

public struct RfqOpened has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    bucket_id: ID,
    write_amount: u64,
    escrow_amount: u64,
    reserve_premium: u64,
    is_put: bool,
}

public struct RfqSettled has copy, drop {
    vault_id: ID,
    ticket_id: ID,
    auction_id: ID,
    bucket_id: ID,
    filled: bool,
    net_premium: u64,
    fee: u64,
    position_id: Option<ID>,
    is_put: bool,
}

public struct PositionRedeemed has copy, drop {
    vault_id: ID,
    bucket_id: ID,
    position_id: ID,
    underlying_out: u64,
    settlement_out: u64,
    is_put: bool,
}

// ═══════════════════════════ call flows ═══════════════════════════

/// Curator escrows `escrow_amount` underlying into a premium auction for
/// `bucket`. One RFQ writes exactly the escrow (1:1 with contracts).
public fun open_call_rfq<U, S, C>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &Bucket<U, S, C>,
    escrow_amount: u64,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    let escrow = vault::take<U>(vault, &mut s, escrow_amount);
    let vault_id = vault::session_vault_id(&s);
    let auction_id = auctions::create_coupled<U, S, OptionsAdapter>(
        OptionsAdapter {},
        escrow,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        vault_id,
        clock,
        ctx,
    );
    let ticket = RfqTicket {
        id: object::new(ctx),
        vault_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount: escrow_amount,
        escrow_amount,
        escrow_type: type_name::with_defining_ids<U>(),
        is_put: false,
    };
    let ticket_id = object::id(&ticket);
    event::emit(RfqOpened {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount: escrow_amount,
        escrow_amount,
        reserve_premium,
        is_put: false,
    });
    vault::put_position(vault, &mut s, ticket);
    vault::end_session(vault, s);
    ticket_id
}

/// Permissionless settle after the auction deadline: winner → write into
/// the bucket, absorb Position + net premium; no winner → escrow back.
public fun settle_call_rfq<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<U, S>,
    bucket: &mut Bucket<U, S, C>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
): Option<ID> {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, false);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize<U, S, OptionsAdapter>(OptionsAdapter {}, auction, clock);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (_bidder, call_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();
        let (pos, call) = bucket::write_collateralized_balance(bucket, escrow, clock, ctx);
        let position_id = object::id(&pos);
        transfer::public_transfer(call, call_recipient);
        vault::put_position(vault, &mut s, pos);
        vault::put<S>(vault, &mut s, net);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: true,
            net_premium,
            fee,
            position_id: option::some(position_id),
            is_put: false,
        });
        vault::end_session(vault, s);
        return option::some(position_id)
    } else {
        winner.destroy_none();
        vault::put<U>(vault, &mut s, escrow);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: false,
            net_premium: 0,
            fee: 0,
            position_id: option::none(),
            is_put: false,
        });
    };
    vault::end_session(vault, s);
    option::none()
}

/// Recovery settle when the bucket died under the auction (expired or
/// invalidated): refund the best bid, reclaim the escrow.
public fun settle_call_rfq_expired<U, S, C>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<U, S>,
    bucket: &Bucket<U, S, C>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        clock.timestamp_ms() >= bucket::expiry_ms(bucket) || bucket::invalidated(bucket),
        E_BUCKET_LIVE,
    );
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, false);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize_early<U, S, OptionsAdapter>(OptionsAdapter {}, auction);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    vault::put<U>(vault, &mut s, escrow);
    event::emit(RfqSettled {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id,
        filled: false,
        net_premium: 0,
        fee: 0,
        position_id: option::none(),
        is_put: false,
    });
    vault::end_session(vault, s);
}

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

/// Curator escrows exact cash collateral for `write_amount` puts.
public fun open_put_rfq<U, S, P>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    bucket: &PutBucket<U, S, P>,
    write_amount: u64,
    reserve_premium: u64,
    duration_ms: u64,
    snipe_window_ms: u64,
    snipe_extension_ms: u64,
    max_extension_ms: u64,
    min_increment_bps: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let escrow_amount = put_bucket::required_collateral(bucket, write_amount);
    let mut s = vault::begin_session(vault, cap, reg, OptionsAdapter {});
    let escrow = vault::take<S>(vault, &mut s, escrow_amount);
    let vault_id = vault::session_vault_id(&s);
    let auction_id = auctions::create_coupled<S, S, OptionsAdapter>(
        OptionsAdapter {},
        escrow,
        reserve_premium,
        duration_ms,
        snipe_window_ms,
        snipe_extension_ms,
        max_extension_ms,
        min_increment_bps,
        vault_id,
        clock,
        ctx,
    );
    let ticket = RfqTicket {
        id: object::new(ctx),
        vault_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount,
        escrow_amount,
        escrow_type: type_name::with_defining_ids<S>(),
        is_put: true,
    };
    let ticket_id = object::id(&ticket);
    event::emit(RfqOpened {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id: object::id(bucket),
        write_amount,
        escrow_amount,
        reserve_premium,
        is_put: true,
    });
    vault::put_position(vault, &mut s, ticket);
    vault::end_session(vault, s);
    ticket_id
}

public fun settle_put_rfq<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<S, S>,
    bucket: &mut PutBucket<U, S, P>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
): Option<ID> {
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let write_amount = ticket.write_amount;
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, true);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize<S, S, OptionsAdapter>(OptionsAdapter {}, auction, clock);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (_bidder, put_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();
        let (pos, put) =
            put_bucket::write_collateralized_balance(bucket, escrow, write_amount, clock, ctx);
        let position_id = object::id(&pos);
        transfer::public_transfer(put, put_recipient);
        vault::put_position(vault, &mut s, pos);
        vault::put<S>(vault, &mut s, net);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: true,
            net_premium,
            fee,
            position_id: option::some(position_id),
            is_put: true,
        });
        vault::end_session(vault, s);
        return option::some(position_id)
    } else {
        winner.destroy_none();
        vault::put<S>(vault, &mut s, escrow);
        event::emit(RfqSettled {
            vault_id,
            ticket_id,
            auction_id,
            bucket_id,
            filled: false,
            net_premium: 0,
            fee: 0,
            position_id: option::none(),
            is_put: true,
        });
    };
    vault::end_session(vault, s);
    option::none()
}

public fun settle_put_rfq_expired<U, S, P>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    ticket_id: ID,
    auction: Auction<S, S>,
    bucket: &PutBucket<U, S, P>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        clock.timestamp_ms() >= put_bucket::expiry_ms(bucket) || put_bucket::invalidated(bucket),
        E_BUCKET_LIVE,
    );
    let mut s = vault::begin_crank_session(vault, reg, OptionsAdapter {});
    let ticket = vault::take_position<RfqTicket>(vault, &mut s, ticket_id);
    let (auction_id, bucket_id) = check_and_burn_ticket(&s, ticket, &auction, true);
    assert!(bucket_id == object::id(bucket), E_BUCKET_MISMATCH);

    let (mut winner, escrow, _receipt) =
        auctions::finalize_early<S, S, OptionsAdapter>(OptionsAdapter {}, auction);
    let vault_id = vault::session_vault_id(&s);
    if (winner.is_some()) {
        let (bidder, _recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    vault::put<S>(vault, &mut s, escrow);
    event::emit(RfqSettled {
        vault_id,
        ticket_id,
        auction_id,
        bucket_id,
        filled: false,
        net_premium: 0,
        fee: 0,
        position_id: option::none(),
        is_put: true,
    });
    vault::end_session(vault, s);
}

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

/// An open RFQ marks at escrow cost (premium upside never marked).
public fun appraise_rfq_ticket<E>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: &mut Appraisal,
    ticket_id: ID,
    att: Option<PriceAttestation>,
    clock: &Clock,
) {
    let ticket: &RfqTicket = vault::borrow_position(vault, ticket_id);
    assert!(type_name::with_defining_ids<E>() == ticket.escrow_type, E_ESCROW_TYPE_MISMATCH);
    let value = value_in_deposit(vault, cfg, ticket.escrow_type, ticket.escrow_amount, att, clock);
    assert!(value <= (std::u64::max_value!() as u128), E_VALUE_OVERFLOW);
    vault::record_position_value(vault, appraisal, OptionsAdapter {}, ticket_id, value as u64);
}

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

/// Validates a ticket against its auction + session vault and burns it.
fun check_and_burn_ticket<E, B>(
    s: &trading_vault::vault::Session,
    ticket: RfqTicket,
    auction: &Auction<E, B>,
    expect_put: bool,
): (ID, ID) {
    let RfqTicket {
        id,
        vault_id,
        auction_id,
        bucket_id,
        write_amount: _,
        escrow_amount: _,
        escrow_type: _,
        is_put,
    } = ticket;
    id.delete();
    assert!(vault_id == vault::session_vault_id(s), E_WRONG_TICKET);
    assert!(auction_id == object::id(auction), E_WRONG_TICKET);
    assert!(auctions::origin(auction) == vault_id, E_WRONG_TICKET);
    assert!(is_put == expect_put, E_WRONG_TICKET);
    (auction_id, bucket_id)
}

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

// ══════════════════════════════ getters ══════════════════════════════

public fun ticket_auction_id(t: &RfqTicket): ID { t.auction_id }

public fun ticket_bucket_id(t: &RfqTicket): ID { t.bucket_id }

public fun ticket_write_amount(t: &RfqTicket): u64 { t.write_amount }

public fun ticket_escrow_amount(t: &RfqTicket): u64 { t.escrow_amount }

public fun ticket_is_put(t: &RfqTicket): bool { t.is_put }
