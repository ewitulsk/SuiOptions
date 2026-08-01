/// DEPRECATED (SO-332) — this package is no longer published and nothing
/// off-chain drives it. Kept in-tree as a reference implementation; see
/// `contracts/vault/DEPRECATED.md` before touching anything here. The live
/// curated-vault product is `contracts/trading-vault`.
///
/// Covered-call vault (docs/vault-implementation-guide/03-vault-contract.md):
/// Ribbon-style weekly rounds over the on-chain RFQ.
///
/// Principles (doc 03 §1):
/// 1. **Permissionless lifecycle** — every round-lifecycle function is a
///    public crank with full on-chain validation; the keeper has zero
///    privileges. Admin only creates the vault, tunes config (applied at
///    the next finalize), and pauses new deposits — it can never move
///    user funds or skip the state machine.
/// 2. **The state machine is the spec** — each crank is legal in exactly
///    one phase; transitions are triggered by time + completed work.
/// 3. **Oracle-bounded discretion** — wherever a crank has a degree of
///    freedom (which bucket, what reserve, what swap price), Pyth bounds
///    it so the freedom can't be abused.
///
/// Accounting follows the reference implementation in
/// `rust-backend/crates/vault-sim/src/ledger.rs` unit-for-unit: u128 pps
/// at `PPS_SCALE`, floor on share mint and withdraw, the
/// profitable-round fee gate with the mgmt-first/perf-clamped cap, queue
/// ordering (withdrawals then deposits at `pps[r]`), the receipt-round
/// convention (round `r` converts at `pps[r − 1]`), and the aggregate
/// share mint at finalize so unclaimed deposit shares count toward
/// supply.
///
/// Proceeds conversion goes through an on-chain swap auction
/// (`open_swap_rfq` / `settle_swap_rfq`, `swap_auction.move`): the vault
/// escrows its settlement proceeds, market makers bid underlying for it,
/// and the winning rate must clear a *fresh* Pyth cross less
/// `max_swap_slippage_bps` at settle — no DEX, no pinned pool, and the
/// crank still chooses nothing. A round can't finalize until its
/// proceeds convert, so an empty auction simply stalls the round until a
/// market maker bids in-band (no value can leak in the meantime).
module options_vault::vault;

use std::type_name;
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin, TreasuryCap};
use sui::object_table::{Self, ObjectTable};
use sui::table::{Self, Table};

use pyth::price_info::PriceInfoObject;

use auction::auction::{Self as auctions, Auction};
use options_core::admin::{AdminCap, ProtocolConfig};
use options_core::bucket::{Self, Bucket};
use options_core::errors as core_errors;
use options_core::position::{Self, Position};
use options_core::treasury::{Self, Treasury};

use options_vault::errors;
use options_vault::events;
use options_vault::oracle;

/// Settle authority for the vault's coupled auctions (RFQ slices and
/// proceeds swaps): only this module can mint it, so only the vault's own
/// settle cranks can resolve them and absorb the balances in-place.
public struct VaultAuth has drop {}

/// Price-per-share fixed point: pps of `PPS_SCALE` ⇒ 1 share == 1
/// underlying smallest-unit. Mirrors `vault-sim::types::PPS_SCALE`.
const PPS_SCALE: u128 = 1_000_000_000_000;

/// 365-day year for the mgmt-fee proration (mirrors vault-sim).
const YEAR_MS: u128 = 31_536_000_000;

/// Settle-crank headroom an auction needs before bucket expiry; mirrors
/// `rfq::SETTLE_BUFFER_MS`.
const SETTLE_BUFFER_MS: u64 = 600_000;

const BPS_DENOM: u128 = 10_000;

// Config hard caps (doc 03 §9).
const MAX_MGMT_FEE_BPS: u64 = 500;
const MAX_PERF_FEE_BPS: u64 = 3_000;
const MAX_SWAP_SLIPPAGE_BPS: u64 = 500;

public enum Phase has copy, drop, store {
    /// Bucket expired (or genesis): redeeming positions, swapping
    /// proceeds, waiting for finalize.
    Settling,
    /// Round live: bucket may be selected, RFQs opened/settled until
    /// `selling_ends_ms`, then hold to expiry.
    Active,
}

public struct VaultConfig has copy, drop, store {
    // Fees, charged only on profitable rounds (§8).
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,

    // Round shape.
    round_ms: u64,
    selling_window_ms: u64,

    // Bucket-selection guardrails vs Pyth spot (§6).
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
    min_expiry_lead_ms: u64,
    max_expiry_lead_ms: u64,

    // RFQ slice guardrails.
    min_reserve_premium_bps: u64,
    max_slice_amount: u64,
    max_open_rfqs: u64,
    rfq_duration_ms: u64,
    rfq_snipe_window_ms: u64,
    rfq_snipe_extension_ms: u64,
    rfq_max_extension_ms: u64,
    rfq_min_increment_bps: u64,

    // Proceeds / swap policy.
    hold_premium_in_settlement: bool,
    /// Band the swap auction's winning rate must clear at settle: the
    /// fill must deliver ≥ Pyth value × (1 − max_swap_slippage_bps).
    /// Also seeds each auction's reserve floor at open.
    max_swap_slippage_bps: u64,

    // Oracle pinning.
    underlying_feed_id: vector<u8>,
    settlement_feed_id: vector<u8>,
    max_price_age_secs: u64,
    max_conf_bps: u64,
    underlying_decimals: u8,
    settlement_decimals: u8,
}

public struct Vault<phantom Underlying, phantom Settlement, phantom VShare> has key {
    id: UID,
    config: VaultConfig,
    /// Stashed by `update_config`, applied at the next finalize so the
    /// admin cannot change rules mid-round.
    pending_config: Option<VaultConfig>,

    // ---- round state machine ----
    round: u64,
    phase: Phase,
    current_bucket: Option<ID>,
    /// Expiry of `current_bucket`; 0 when none.
    current_expiry_ms: u64,
    /// `open_rfq` forbidden after this.
    selling_ends_ms: u64,
    /// Coupled proceeds-swap auctions created and not yet settled. Must
    /// be 0 before finalize, like `open_rfqs`.
    open_swap_rfqs: u64,

    // ---- share accounting ----
    share_treasury: TreasuryCap<VShare>,
    /// pps[r], set exactly once when round r finalizes.
    pps: Table<u64, u128>,
    /// Shares minted in aggregate at finalize for that round's deposits,
    /// awaiting `claim_shares`. Counted in total supply (so unclaimed
    /// depositors earn their rounds' P&L); per-receipt floor dust stays
    /// here, favoring the vault.
    claimable_shares: Balance<VShare>,
    /// Shares escrowed by `initiate_withdraw`; burned at next finalize.
    queued_withdraw_shares: Balance<VShare>,

    // ---- balances ----
    deployable: Balance<Underlying>,
    pending_deposits: Balance<Underlying>,
    /// Premium + exercise proceeds awaiting swap (or Pyth valuation
    /// under the hold-premium policy).
    proceeds_settlement: Balance<Settlement>,
    /// Fully reserved for completed WithdrawReceipts.
    withdrawal_pool: Balance<Underlying>,

    // ---- per-round working state ----
    positions: ObjectTable<u64, Position>,
    positions_head: u64,
    positions_tail: u64,
    open_rfqs: u64,
    /// Net-of-protocol-fee premium collected this round, settlement units.
    round_premium_collected: u64,
    /// Cumulative swap legs this round, for the premium→underlying
    /// conversion at the round's realized rate.
    round_swap_settlement_out: u64,
    round_swap_underlying_in: u64,

    paused_deposits: bool,
}

/// Claim ticket for a queued deposit: claimable at `pps[round − 1]` once
/// that price exists (doc 03 §5.2).
public struct DepositReceipt has key, store {
    id: UID,
    vault_id: ID,
    round: u64,
    amount: u64,
}

/// Two-step withdrawal ticket: pays `shares × pps[round] / PPS_SCALE`
/// once round `round` finalizes.
public struct WithdrawReceipt has key, store {
    id: UID,
    vault_id: ID,
    round: u64,
    shares: u64,
}

// ════════════════════════════════ admin ════════════════════════════════

/// Create and share a vault. The share `TreasuryCap` must be fresh (zero
/// supply) so supply == outstanding shares from genesis, like
/// `create_bucket`'s cap check.
public fun create_vault<Underlying, Settlement, VShare>(
    _: &AdminCap,
    share_treasury: TreasuryCap<VShare>,
    config: VaultConfig,
    ctx: &mut TxContext,
): ID {
    assert!(coin::total_supply(&share_treasury) == 0, core_errors::treasury_cap_not_fresh());
    validate_config(&config);
    let vault = Vault<Underlying, Settlement, VShare> {
        id: object::new(ctx),
        config,
        pending_config: option::none(),
        round: 0,
        phase: Phase::Settling,
        current_bucket: option::none(),
        current_expiry_ms: 0,
        selling_ends_ms: 0,
        open_swap_rfqs: 0,
        share_treasury,
        pps: table::new(ctx),
        claimable_shares: balance::zero(),
        queued_withdraw_shares: balance::zero(),
        deployable: balance::zero(),
        pending_deposits: balance::zero(),
        proceeds_settlement: balance::zero(),
        withdrawal_pool: balance::zero(),
        positions: object_table::new(ctx),
        positions_head: 0,
        positions_tail: 0,
        open_rfqs: 0,
        round_premium_collected: 0,
        round_swap_settlement_out: 0,
        round_swap_underlying_in: 0,
        paused_deposits: false,
    };
    let vault_id = object::id(&vault);
    events::emit_vault_created(
        vault_id,
        type_name::with_defining_ids<Underlying>(),
        type_name::with_defining_ids<Settlement>(),
        type_name::with_defining_ids<VShare>(),
        vault.config.mgmt_fee_bps_annual,
        vault.config.perf_fee_bps,
        vault.config.round_ms,
        vault.config.selling_window_ms,
        vault.config.min_strike_bps_over_spot,
        vault.config.max_strike_bps_over_spot,
    );
    transfer::share_object(vault);
    vault_id
}

/// Plain-field constructor so PTBs (and tests) can assemble the config.
public fun new_config(
    mgmt_fee_bps_annual: u64,
    perf_fee_bps: u64,
    round_ms: u64,
    selling_window_ms: u64,
    min_strike_bps_over_spot: u64,
    max_strike_bps_over_spot: u64,
    min_expiry_lead_ms: u64,
    max_expiry_lead_ms: u64,
    min_reserve_premium_bps: u64,
    max_slice_amount: u64,
    max_open_rfqs: u64,
    rfq_duration_ms: u64,
    rfq_snipe_window_ms: u64,
    rfq_snipe_extension_ms: u64,
    rfq_max_extension_ms: u64,
    rfq_min_increment_bps: u64,
    hold_premium_in_settlement: bool,
    max_swap_slippage_bps: u64,
    underlying_feed_id: vector<u8>,
    settlement_feed_id: vector<u8>,
    max_price_age_secs: u64,
    max_conf_bps: u64,
    underlying_decimals: u8,
    settlement_decimals: u8,
): VaultConfig {
    VaultConfig {
        mgmt_fee_bps_annual,
        perf_fee_bps,
        round_ms,
        selling_window_ms,
        min_strike_bps_over_spot,
        max_strike_bps_over_spot,
        min_expiry_lead_ms,
        max_expiry_lead_ms,
        min_reserve_premium_bps,
        max_slice_amount,
        max_open_rfqs,
        rfq_duration_ms,
        rfq_snipe_window_ms,
        rfq_snipe_extension_ms,
        rfq_max_extension_ms,
        rfq_min_increment_bps,
        hold_premium_in_settlement,
        max_swap_slippage_bps,
        underlying_feed_id,
        settlement_feed_id,
        max_price_age_secs,
        max_conf_bps,
        underlying_decimals,
        settlement_decimals,
    }
}

fun validate_config(config: &VaultConfig) {
    assert!(config.mgmt_fee_bps_annual <= MAX_MGMT_FEE_BPS, errors::vault_config_invalid());
    assert!(config.perf_fee_bps <= MAX_PERF_FEE_BPS, errors::vault_config_invalid());
    assert!(config.max_swap_slippage_bps <= MAX_SWAP_SLIPPAGE_BPS, errors::vault_config_invalid());
    assert!(
        config.min_strike_bps_over_spot < config.max_strike_bps_over_spot,
        errors::vault_config_invalid(),
    );
    assert!(
        config.min_expiry_lead_ms < config.max_expiry_lead_ms,
        errors::vault_config_invalid(),
    );
    assert!(config.round_ms > 0 && config.selling_window_ms > 0, errors::vault_config_invalid());
    assert!(config.max_slice_amount > 0 && config.max_open_rfqs > 0, errors::vault_config_invalid());
    assert!(config.max_price_age_secs > 0 && config.max_conf_bps > 0, errors::vault_config_invalid());
}

/// Stash a config change; it takes effect at the next `finalize_round`
/// so rules can't change mid-flight (doc 03 §9).
public fun update_config<U, S, V>(
    _: &AdminCap,
    vault: &mut Vault<U, S, V>,
    new_config: VaultConfig,
) {
    validate_config(&new_config);
    vault.pending_config = option::some(new_config);
    events::emit_vault_config_updated(object::id(vault), vault.round);
}

/// Admin escape hatch for an immediate oracle-feed migration. Unlike
/// `update_config` (which defers to `finalize_round`), this rewrites the
/// active feed ids in place. The lifecycle cranks resolve the oracle from
/// the *active* config (`select_bucket`/`finalize_round` → `vault_spot`),
/// so a vault pinned to a feed set with no `PriceInfoObject` on this
/// network is otherwise wedged: the crank that would apply replacement
/// feeds via `update_config` can never resolve the old ones to run. This
/// breaks that deadlock. Restricted to `Settling` so it can't move the
/// reference market a live round is already selling against; any queued
/// `pending_config` is kept in sync so the next finalize can't revert it.
public fun update_oracle_feeds<U, S, V>(
    _: &AdminCap,
    vault: &mut Vault<U, S, V>,
    underlying_feed_id: vector<u8>,
    settlement_feed_id: vector<u8>,
) {
    assert!(vault.phase == Phase::Settling, errors::vault_wrong_phase());
    vault.config.underlying_feed_id = underlying_feed_id;
    vault.config.settlement_feed_id = settlement_feed_id;
    if (vault.pending_config.is_some()) {
        let pending = vault.pending_config.borrow_mut();
        pending.underlying_feed_id = underlying_feed_id;
        pending.settlement_feed_id = settlement_feed_id;
    };
    events::emit_vault_config_updated(object::id(vault), vault.round);
}

public fun pause_deposits<U, S, V>(_: &AdminCap, vault: &mut Vault<U, S, V>) {
    vault.paused_deposits = true;
    events::emit_vault_deposits_paused(object::id(vault), true);
}

public fun unpause_deposits<U, S, V>(_: &AdminCap, vault: &mut Vault<U, S, V>) {
    vault.paused_deposits = false;
    events::emit_vault_deposits_paused(object::id(vault), false);
}

// ═══════════════════════════ user functions ═══════════════════════════

/// Queue a deposit for the next round (doc 03 §5.1). Never exposed to
/// the current round's P&L.
public fun deposit<U, S, V>(
    vault: &mut Vault<U, S, V>,
    coin: Coin<U>,
    ctx: &mut TxContext,
): DepositReceipt {
    assert!(!vault.paused_deposits, errors::vault_deposits_paused());
    let amount = coin.value();
    assert!(amount > 0, core_errors::zero_amount());
    vault.pending_deposits.join(coin.into_balance());
    let round = vault.round + 1;
    events::emit_vault_deposit(object::id(vault), ctx.sender(), round, amount);
    DepositReceipt { id: object::new(ctx), vault_id: object::id(vault), round, amount }
}

/// Convert a deposit receipt at `pps[round − 1]` (doc 03 §5.2). The
/// shares were minted in aggregate at that round's finalize; this just
/// allocates them.
public fun claim_shares<U, S, V>(
    vault: &mut Vault<U, S, V>,
    receipt: DepositReceipt,
    ctx: &mut TxContext,
): Coin<V> {
    let DepositReceipt { id, vault_id, round, amount } = receipt;
    id.delete();
    assert!(vault_id == object::id(vault), errors::vault_receipt_round_mismatch());
    assert!(vault.pps.contains(round - 1), errors::vault_round_not_finalized());
    let pps = vault.pps[round - 1];
    let shares = (((amount as u128) * PPS_SCALE) / pps) as u64;
    events::emit_shares_claimed(object::id(vault), ctx.sender(), round, amount, shares);
    coin::from_balance(vault.claimable_shares.split(shares), ctx)
}

/// Escrow shares for a two-step withdrawal (doc 03 §5.3). The escrowed
/// shares stay exposed to the current round's P&L — Ribbon semantics.
public fun initiate_withdraw<U, S, V>(
    vault: &mut Vault<U, S, V>,
    shares: Coin<V>,
    ctx: &mut TxContext,
): WithdrawReceipt {
    let amount = shares.value();
    assert!(amount > 0, core_errors::zero_amount());
    vault.queued_withdraw_shares.join(shares.into_balance());
    let round = vault.round;
    events::emit_withdraw_initiated(object::id(vault), ctx.sender(), round, amount);
    WithdrawReceipt { id: object::new(ctx), vault_id: object::id(vault), round, shares: amount }
}

/// Pay out a finalized withdrawal at its locked pps (doc 03 §5.4), from
/// the pool finalize fully funded.
public fun complete_withdraw<U, S, V>(
    vault: &mut Vault<U, S, V>,
    receipt: WithdrawReceipt,
    ctx: &mut TxContext,
): Coin<U> {
    let WithdrawReceipt { id, vault_id, round, shares } = receipt;
    id.delete();
    assert!(vault_id == object::id(vault), errors::vault_receipt_round_mismatch());
    assert!(vault.pps.contains(round), errors::vault_round_not_finalized());
    let pps = vault.pps[round];
    let owed = (((shares as u128) * pps) / PPS_SCALE) as u64;
    events::emit_withdraw_completed(object::id(vault), ctx.sender(), round, shares, owed);
    coin::from_balance(vault.withdrawal_pool.split(owed), ctx)
}

/// Cancel a deposit whose round hasn't started (doc 03 §5.5).
public fun instant_withdraw_pending<U, S, V>(
    vault: &mut Vault<U, S, V>,
    receipt: DepositReceipt,
    ctx: &mut TxContext,
): Coin<U> {
    let DepositReceipt { id, vault_id, round, amount } = receipt;
    id.delete();
    assert!(vault_id == object::id(vault), errors::vault_receipt_round_mismatch());
    assert!(round > vault.round, errors::vault_receipt_round_mismatch());
    events::emit_instant_withdraw(object::id(vault), ctx.sender(), vault.round, amount);
    coin::from_balance(vault.pending_deposits.split(amount), ctx)
}

// ════════════════════ lifecycle cranks (permissionless) ════════════════════

/// Redeem the next position in the FIFO — one per call, bounded gas
/// (doc 03 §7.1). The first call after expiry flips the phase to
/// Settling.
public fun crank_redeem<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &mut Bucket<U, S, C>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    maybe_enter_settling(vault, clock);
    assert!(vault.phase == Phase::Settling, errors::vault_wrong_phase());
    assert!(
        vault.current_bucket.contains(&object::id(bucket)),
        errors::vault_bucket_not_selected(),
    );
    assert!(vault.positions_head < vault.positions_tail, errors::vault_positions_pending());

    let pos = vault.positions.remove(vault.positions_head);
    vault.positions_head = vault.positions_head + 1;
    let position_id = object::id(&pos);
    let (underlying, settlement) = bucket::redeem_position(bucket, pos, clock, ctx);
    events::emit_vault_position_redeemed(
        object::id(vault),
        vault.round,
        position_id,
        underlying.value(),
        settlement.value(),
    );
    vault.deployable.join(underlying.into_balance());
    vault.proceeds_settlement.join(settlement.into_balance());
}

/// Escrow up to `amount_s` of the vault's settlement proceeds into a
/// coupled swap auction (doc 03 §7.3). MMs then bid underlying for it;
/// `settle_swap_rfq` resolves it after the deadline. The reserve floor is
/// the Pyth value of the escrowed proceeds less `max_swap_slippage_bps`;
/// the *binding* band check is re-applied against a fresh cross at
/// settle. Permissionless and legal in any phase (mid-round premium
/// conversion under the non-hold-premium policy, or post-expiry exercise
/// proceeds in Settling).
public fun open_swap_rfq<U, S, V>(
    vault: &mut Vault<U, S, V>,
    amount_s: u64,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let (spot, spot_scale) = vault_spot(vault, underlying_info, settlement_info, clock);
    open_swap_rfq_internal(vault, amount_s, spot, spot_scale, clock, ctx)
}

fun open_swap_rfq_internal<U, S, V>(
    vault: &mut Vault<U, S, V>,
    amount_s: u64,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(vault.open_swap_rfqs < vault.config.max_open_rfqs, errors::vault_too_many_rfqs());
    let s_in = amount_s.min(vault.proceeds_settlement.value());
    assert!(s_in > 0, core_errors::zero_amount());

    // Reserve = the band floor on the open-time cross. > 0 guards against
    // dust proceeds that round to nothing (re-checked fresh at settle).
    let reserve = swap_floor(vault, s_in, spot, spot_scale);
    assert!(reserve > 0, errors::vault_proceeds_unswapped());

    let settlement = vault.proceeds_settlement.split(s_in);
    // The swap sells settlement for underlying bids: Auction<S, U>. `U`
    // only appears in the phantom bid leg, so annotate it explicitly.
    let swap_id = auctions::create_coupled<S, U, VaultAuth>(
        VaultAuth {},
        settlement,
        reserve,
        vault.config.rfq_duration_ms,
        vault.config.rfq_snipe_window_ms,
        vault.config.rfq_snipe_extension_ms,
        vault.config.rfq_max_extension_ms,
        vault.config.rfq_min_increment_bps,
        object::id(vault),
        clock,
        ctx,
    );
    vault.open_swap_rfqs = vault.open_swap_rfqs + 1;
    swap_id
}

/// Resolve one of the vault's swap auctions. The winning bid is re-checked
/// against a *fresh* Pyth cross: if it still clears the band, the winner
/// takes the settlement and the vault absorbs the underlying (recording
/// the round's realized swap rate for the perf fee); if the price moved
/// out of band, or there were no bids, the settlement returns to proceeds
/// for re-auction and the bid (if any) is refunded. Permissionless — a
/// fresh Pyth push is required in the same PTB (the keeper prepends it).
public fun settle_swap_rfq<U, S, V>(
    vault: &mut Vault<U, S, V>,
    auction: Auction<S, U>,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let (spot, spot_scale) = vault_spot(vault, underlying_info, settlement_info, clock);
    settle_swap_rfq_internal(vault, auction, spot, spot_scale, clock, ctx)
}

fun settle_swap_rfq_internal<U, S, V>(
    vault: &mut Vault<U, S, V>,
    auction: Auction<S, U>,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(auctions::origin(&auction) == object::id(vault), errors::vault_wrong_origin());
    let (mut winner, settlement, receipt) =
        auctions::finalize<S, U, VaultAuth>(VaultAuth {}, auction, clock);
    let swap_id = auctions::receipt_auction_id(&receipt);
    let amount_s = auctions::receipt_amount(&receipt);

    if (winner.is_some()) {
        let (bidder, token_recipient, underlying) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let u_in = underlying.value();
        let u_min = swap_floor(vault, amount_s, spot, spot_scale);
        if (u_min > 0 && u_in >= u_min) {
            // Band cleared: hand the settlement to the winner's chosen
            // recipient, absorb the underlying, record the realized rate
            // (premium→underlying).
            transfer::public_transfer(coin::from_balance(settlement, ctx), token_recipient);
            vault.deployable.join(underlying);
            vault.round_swap_settlement_out = vault.round_swap_settlement_out + amount_s;
            vault.round_swap_underlying_in = vault.round_swap_underlying_in + u_in;
            events::emit_swap_rfq_settled(
                swap_id,
                object::id(vault),
                vault.round,
                bidder,
                amount_s,
                u_in,
            );
        } else {
            // Price moved out of band before settle: no swap. Refund the
            // bidder, return the settlement to proceeds; the keeper
            // re-opens (the round stalls until an in-band bid lands).
            transfer::public_transfer(coin::from_balance(underlying, ctx), bidder);
            vault.proceeds_settlement.join(settlement);
            events::emit_swap_rfq_unfilled(swap_id, object::id(vault), vault.round, amount_s);
        }
    } else {
        winner.destroy_none();
        vault.proceeds_settlement.join(settlement);
        events::emit_swap_rfq_unfilled(swap_id, object::id(vault), vault.round, amount_s);
    };
    vault.open_swap_rfqs = vault.open_swap_rfqs - 1;
}

/// The fresh-Pyth band floor for converting `amount_s` settlement: the
/// minimum underlying a fill must deliver = Pyth value × (1 − slippage).
/// Floors against the vault (settlement_to_underlying floors).
fun swap_floor<U, S, V>(
    vault: &Vault<U, S, V>,
    amount_s: u64,
    spot: u128,
    spot_scale: u8,
): u64 {
    let u_fair = settlement_to_underlying(amount_s, spot, spot_scale);
    ((
        (u_fair as u128) * (BPS_DENOM - (vault.config.max_swap_slippage_bps as u128))
            / BPS_DENOM
    ) as u64)
}

/// The accounting heart (doc 03 §7.4): lock pps, charge fees on
/// profitable rounds, process queues (withdrawals then deposits), and
/// activate the next round.
public fun finalize_round<U, S, V>(
    vault: &mut Vault<U, S, V>,
    treasury: &mut Treasury,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let (spot, spot_scale) = vault_spot(vault, underlying_info, settlement_info, clock);
    finalize_round_internal(vault, treasury, spot, spot_scale, clock, ctx)
}

fun finalize_round_internal<U, S, V>(
    vault: &mut Vault<U, S, V>,
    treasury: &mut Treasury,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    _ctx: &mut TxContext,
) {
    maybe_enter_settling(vault, clock);
    assert!(vault.phase == Phase::Settling, errors::vault_wrong_phase());
    assert!(vault.positions_head == vault.positions_tail, errors::vault_positions_pending());
    assert!(vault.open_rfqs == 0, errors::vault_rfqs_open());
    assert!(vault.open_swap_rfqs == 0, errors::vault_rfqs_open());

    // Proceeds policy: under hold-premium the residual settlement is
    // valued at the Pyth cross; otherwise it must have been swapped.
    let residual_s = vault.proceeds_settlement.value();
    let valued_s = if (vault.config.hold_premium_in_settlement) {
        settlement_to_underlying(residual_s, spot, spot_scale)
    } else {
        assert!(residual_s == 0, errors::vault_proceeds_unswapped());
        0
    };

    let round = vault.round;
    let aum = vault.deployable.value() + valued_s;
    let shares = total_share_supply(vault);
    let pps_prev = if (round == 0) { PPS_SCALE } else { vault.pps[round - 1] };
    let pps_gross = if (shares == 0) {
        pps_prev
    } else {
        ((aum as u128) * PPS_SCALE) / (shares as u128)
    };

    // Premium in underlying terms, for the perf fee: at the round's
    // realized swap rate, or the Pyth cross under hold-premium.
    let premium_s = vault.round_premium_collected;
    let premium_u = if (vault.config.hold_premium_in_settlement) {
        settlement_to_underlying(premium_s, spot, spot_scale)
    } else if (vault.round_swap_settlement_out > 0) {
        ((
            (premium_s as u128) * (vault.round_swap_underlying_in as u128)
                / (vault.round_swap_settlement_out as u128)
        ) as u64)
    } else {
        0
    };

    // Fees only on profitable rounds; mgmt first, perf absorbs the
    // profit cap (mirrors vault-sim::ledger).
    let (mgmt_fee, perf_fee) = if (pps_gross > pps_prev) {
        let mgmt = (aum as u128) * (vault.config.mgmt_fee_bps_annual as u128)
            * (vault.config.round_ms as u128) / (BPS_DENOM * YEAR_MS);
        let perf = (premium_u as u128) * (vault.config.perf_fee_bps as u128) / BPS_DENOM;
        let baseline = ((shares as u128) * pps_prev) / PPS_SCALE;
        let profit = (aum as u128) - baseline;
        let mgmt_charged = mgmt.min(profit);
        let perf_charged = perf.min(profit - mgmt_charged);
        ((mgmt_charged as u64), (perf_charged as u64))
    } else {
        (0, 0)
    };
    let fees = mgmt_fee + perf_fee;
    if (fees > 0) {
        treasury::deposit_balance(treasury, vault.deployable.split(fees));
    };
    events::emit_vault_fees_charged(object::id(vault), round, mgmt_fee, perf_fee);

    // Lock the round price.
    let net_aum = aum - fees;
    let pps = if (shares == 0) {
        pps_prev
    } else {
        ((net_aum as u128) * PPS_SCALE) / (shares as u128)
    };
    vault.pps.add(round, pps);

    // Queues, in order: withdrawals first, then deposits, both at pps[round].
    let shares_burned = vault.queued_withdraw_shares.value();
    let withdrawals_owed = (((shares_burned as u128) * pps) / PPS_SCALE) as u64;
    // Withdrawals pay underlying: under hold-premium a large settlement
    // carry could leave deployable short — force a fill first.
    assert!(vault.deployable.value() >= withdrawals_owed, errors::vault_proceeds_unswapped());
    vault.withdrawal_pool.join(vault.deployable.split(withdrawals_owed));
    let burn_coin = coin::from_balance(vault.queued_withdraw_shares.withdraw_all(), _ctx);
    coin::burn(&mut vault.share_treasury, burn_coin);

    let deposits_processed = vault.pending_deposits.value();
    let shares_minted = (((deposits_processed as u128) * PPS_SCALE) / pps) as u64;
    vault.deployable.join(vault.pending_deposits.withdraw_all());
    if (shares_minted > 0) {
        let minted = coin::mint(&mut vault.share_treasury, shares_minted, _ctx);
        vault.claimable_shares.join(minted.into_balance());
    };

    // Activate the next round.
    vault.round = round + 1;
    vault.phase = Phase::Active;
    vault.current_bucket = option::none();
    vault.current_expiry_ms = 0;
    vault.selling_ends_ms = 0;
    vault.positions_head = 0;
    vault.positions_tail = 0;
    vault.round_premium_collected = 0;
    vault.round_swap_settlement_out = 0;
    vault.round_swap_underlying_in = 0;
    if (vault.pending_config.is_some()) {
        vault.config = vault.pending_config.extract();
    };

    // Record the active config for this round (post-apply) so off-chain
    // services serve real values instead of hard-coded fees/cadence.
    events::emit_vault_config_applied(
        object::id(vault),
        round,
        vault.config.mgmt_fee_bps_annual,
        vault.config.perf_fee_bps,
        vault.config.round_ms,
        vault.config.selling_window_ms,
        vault.config.min_strike_bps_over_spot,
        vault.config.max_strike_bps_over_spot,
    );

    events::emit_vault_round_finalized(
        object::id(vault),
        round,
        pps,
        aum,
        shares,
        premium_s,
        premium_u,
        withdrawals_owed,
        shares_burned,
        deposits_processed,
        shares_minted,
    );
}

/// Pick the round's bucket (doc 03 §7.5): expiry inside the configured
/// lead window, strike inside the Pyth band. The band bounds — not
/// eliminates — keeper discretion: a hostile keeper can still pick the
/// band-edge strike and rely on a quiet auction clearing at the reserve
/// floor, so `min_reserve_premium_bps` is the real per-slice loss bound
/// (set it accordingly; see the keeper README trust model).
public fun select_bucket<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    _ctx: &mut TxContext,
) {
    let (spot, spot_scale) = vault_spot(vault, underlying_info, settlement_info, clock);
    select_bucket_internal(vault, bucket, spot, spot_scale, clock)
}

fun select_bucket_internal<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
) {
    assert!(vault.phase == Phase::Active, errors::vault_wrong_phase());
    assert!(vault.current_bucket.is_none(), errors::vault_bucket_already_selected());
    assert!(!bucket::invalidated(bucket), core_errors::bucket_invalidated());

    let now = clock.timestamp_ms();
    let expiry = bucket::expiry_ms(bucket);
    assert!(
        expiry >= now + vault.config.min_expiry_lead_ms
            && expiry <= now + vault.config.max_expiry_lead_ms,
        errors::vault_expiry_out_of_band(),
    );

    // Strike band (§6), cross-multiplied so the comparison is exact at
    // both scales: strike/10^ss ⋛ spot/10^os × (1 + bps/10⁴).
    let strike = bucket::strike(bucket);
    let strike_scale = bucket::strike_scale(bucket);
    let lhs = (strike as u256) * pow10_u256(spot_scale) * (BPS_DENOM as u256);
    let rhs_base = (spot as u256) * pow10_u256(strike_scale);
    assert!(
        lhs >= rhs_base * ((BPS_DENOM as u256) + (vault.config.min_strike_bps_over_spot as u256)),
        errors::vault_strike_out_of_band(),
    );
    assert!(
        lhs <= rhs_base * ((BPS_DENOM as u256) + (vault.config.max_strike_bps_over_spot as u256)),
        errors::vault_strike_out_of_band(),
    );

    vault.current_bucket = option::some(object::id(bucket));
    vault.current_expiry_ms = expiry;
    // Cap the selling window so the last possible auction still has room
    // for its full duration + extensions + the settle buffer.
    let auction_room = vault.config.rfq_duration_ms + vault.config.rfq_max_extension_ms
        + SETTLE_BUFFER_MS;
    let hard_cap = expiry - auction_room;
    vault.selling_ends_ms = (now + vault.config.selling_window_ms).min(hard_cap);

    events::emit_vault_bucket_selected(
        object::id(vault),
        vault.round,
        object::id(bucket),
        strike,
        strike_scale,
        expiry,
        vault.selling_ends_ms,
        spot,
        spot_scale,
    );
}

/// Escrow a slice into a coupled RFQ auction (doc 03 §7.6). Slicing
/// strategy is keeper-side policy; the contract enforces the caps and
/// derives the reserve floor from Pyth.
public fun open_rfq<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    slice_amount: u64,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    let (spot, spot_scale) = vault_spot(vault, underlying_info, settlement_info, clock);
    open_rfq_internal(vault, bucket, slice_amount, spot, spot_scale, clock, ctx)
}

fun open_rfq_internal<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    slice_amount: u64,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(vault.phase == Phase::Active, errors::vault_wrong_phase());
    assert!(
        vault.current_bucket.contains(&object::id(bucket)),
        errors::vault_bucket_not_selected(),
    );
    assert!(clock.timestamp_ms() < vault.selling_ends_ms, errors::vault_selling_closed());
    assert!(vault.open_rfqs < vault.config.max_open_rfqs, errors::vault_too_many_rfqs());
    assert!(slice_amount > 0, core_errors::zero_amount());
    assert!(
        slice_amount <= vault.config.max_slice_amount
            && slice_amount <= vault.deployable.value(),
        errors::vault_slice_too_large(),
    );

    // Reserve floor (§6): min_reserve_premium_bps of the slice's spot
    // notional, at least 1 unit. A floor, not a fair price — competition
    // discovers price; this only stops a quiet auction giving calls away.
    let notional = settlement_notional(slice_amount, spot, spot_scale);
    let reserve = ((
        (notional as u128) * (vault.config.min_reserve_premium_bps as u128) / BPS_DENOM
    ) as u64).max(1);

    // Bucket-alive checks the old coupled-RFQ create performed; the
    // selling-window cap (set at `select_bucket`) already guarantees the
    // auction's hard deadline + settle buffer fit before expiry.
    assert!(!bucket::invalidated(bucket), core_errors::bucket_invalidated());
    assert!(clock.timestamp_ms() < bucket::expiry_ms(bucket), core_errors::bucket_expired());

    let collateral = vault.deployable.split(slice_amount);
    let rfq_id = auctions::create_coupled<U, S, VaultAuth>(
        VaultAuth {},
        collateral,
        reserve,
        vault.config.rfq_duration_ms,
        vault.config.rfq_snipe_window_ms,
        vault.config.rfq_snipe_extension_ms,
        vault.config.rfq_max_extension_ms,
        vault.config.rfq_min_increment_bps,
        object::id(vault),
        clock,
        ctx,
    );
    vault.open_rfqs = vault.open_rfqs + 1;
    rfq_id
}

/// Resolve one of the vault's auctions (doc 03 §7.7): the winner gets
/// `Coin<Call>`, the vault absorbs the `Position` and net premium
/// in-place — one transaction, no receiving dance.
public fun settle_rfq<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    auction: Auction<U, S>,
    bucket: &mut Bucket<U, S, C>,
    config: &ProtocolConfig,
    treasury: &mut Treasury,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(auctions::origin(&auction) == object::id(vault), errors::vault_wrong_origin());
    assert!(
        vault.current_bucket.contains(&object::id(bucket)),
        errors::vault_bucket_not_selected(),
    );
    let (mut winner, collateral, receipt) =
        auctions::finalize<U, S, VaultAuth>(VaultAuth {}, auction, clock);
    let auction_id = auctions::receipt_auction_id(&receipt);

    if (winner.is_some()) {
        let (bidder, call_recipient, premium) = auctions::unpack_bid(winner.extract());
        winner.destroy_none();
        let gross_premium = premium.value();
        let (net, fee) = bucket::skim_fee(config, treasury, premium);
        let net_premium = net.value();

        let (pos, call) = bucket::write_collateralized_balance(bucket, collateral, clock, ctx);
        let position_id = object::id(&pos);
        let range_start = position::range_start(&pos);
        let range_end = position::range_end(&pos);
        transfer::public_transfer(call, call_recipient);
        vault.positions.add(vault.positions_tail, pos);
        vault.positions_tail = vault.positions_tail + 1;
        vault.proceeds_settlement.join(net);
        vault.round_premium_collected = vault.round_premium_collected + net_premium;

        events::emit_vault_rfq_settled(
            auction_id,
            object::id(bucket),
            object::id(vault),
            vault.round,
            bidder,
            call_recipient,
            position_id,
            auctions::receipt_amount(&receipt),
            gross_premium,
            fee,
            net_premium,
            range_start,
            range_end,
        );
    } else {
        winner.destroy_none();
        vault.deployable.join(collateral);
        events::emit_vault_rfq_unsold(
            auction_id,
            object::id(bucket),
            object::id(vault),
            vault.round,
            auctions::receipt_amount(&receipt),
            auctions::receipt_reserve_bid(&receipt),
        );
    };
    vault.open_rfqs = vault.open_rfqs - 1;
}

/// Recovery twin of `settle_rfq` (doc 03 §7.2): the bucket died
/// mid-auction, so the write can never execute — refund the bid to the
/// bidder and absorb the collateral back into deployable. Permissionless,
/// so no admin is ever needed to unstick a round; no deadline
/// precondition, since a dead bucket makes the auction moot.
public fun settle_rfq_expired<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    auction: Auction<U, S>,
    bucket: &Bucket<U, S, C>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(auctions::origin(&auction) == object::id(vault), errors::vault_wrong_origin());
    assert!(
        clock.timestamp_ms() >= bucket::expiry_ms(bucket) || bucket::invalidated(bucket),
        core_errors::bucket_not_expired(),
    );
    let (mut winner, collateral, receipt) =
        auctions::finalize_early<U, S, VaultAuth>(VaultAuth {}, auction);
    if (winner.is_some()) {
        let (bidder, _call_recipient, premium) = auctions::unpack_bid(winner.extract());
        transfer::public_transfer(coin::from_balance(premium, ctx), bidder);
    };
    winner.destroy_none();
    vault.deployable.join(collateral);
    vault.open_rfqs = vault.open_rfqs - 1;
    events::emit_vault_rfq_unsold(
        auctions::receipt_auction_id(&receipt),
        object::id(bucket),
        object::id(vault),
        vault.round,
        auctions::receipt_amount(&receipt),
        auctions::receipt_reserve_bid(&receipt),
    );
}

// ════════════════════════════ internals ════════════════════════════

/// Active → Settling once the round's bucket has expired. Rounds that
/// never selected a bucket (`current_expiry_ms == 0`) settle immediately
/// — liveness for zero-deposit / zero-bid rounds and genesis.
fun maybe_enter_settling<U, S, V>(vault: &mut Vault<U, S, V>, clock: &Clock) {
    if (vault.phase == Phase::Active && clock.timestamp_ms() >= vault.current_expiry_ms) {
        vault.phase = Phase::Settling;
    }
}

fun vault_spot<U, S, V>(
    vault: &Vault<U, S, V>,
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    clock: &Clock,
): (u128, u8) {
    oracle::spot_cross(
        underlying_info,
        settlement_info,
        vault.config.underlying_feed_id,
        vault.config.settlement_feed_id,
        vault.config.underlying_decimals,
        vault.config.settlement_decimals,
        vault.config.max_price_age_secs,
        vault.config.max_conf_bps,
        clock,
    )
}

/// amount × spot / 10^spot_scale, round-half-up — `apply_strike`-style
/// settlement notional of an underlying amount.
fun settlement_notional(amount: u64, spot: u128, spot_scale: u8): u64 {
    let divisor = pow10_u256(spot_scale);
    let numerator = (amount as u256) * (spot as u256);
    (((numerator + divisor / 2) / divisor) as u64)
}

/// amount_s × 10^spot_scale / spot, floor — settlement valued in
/// underlying smallest-units (the swap can only round against the
/// vault).
fun settlement_to_underlying(amount_s: u64, spot: u128, spot_scale: u8): u64 {
    if (amount_s == 0) {
        return 0
    };
    (((amount_s as u256) * pow10_u256(spot_scale) / (spot as u256)) as u64)
}

fun pow10_u256(exp: u8): u256 {
    let mut result: u256 = 1;
    let mut i: u8 = 0;
    while (i < exp) {
        result = result * 10;
        i = i + 1;
    };
    result
}

/// Outstanding + queued shares — the denominator round P&L accrues to.
fun total_share_supply<U, S, V>(vault: &Vault<U, S, V>): u64 {
    coin::total_supply(&vault.share_treasury)
}

// ════════════════════════════ getters ════════════════════════════

public fun round<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.round }
public fun is_settling<U, S, V>(vault: &Vault<U, S, V>): bool { vault.phase == Phase::Settling }
public fun current_bucket<U, S, V>(vault: &Vault<U, S, V>): Option<ID> { vault.current_bucket }
public fun current_expiry_ms<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.current_expiry_ms }
public fun selling_ends_ms<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.selling_ends_ms }
public fun open_rfqs<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.open_rfqs }
public fun open_swap_rfqs<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.open_swap_rfqs }
public fun deployable<U, S, V>(vault: &Vault<U, S, V>): u64 { vault.deployable.value() }
public fun pending_deposits<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.pending_deposits.value()
}
public fun proceeds_settlement<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.proceeds_settlement.value()
}
public fun withdrawal_pool<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.withdrawal_pool.value()
}
public fun claimable_shares<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.claimable_shares.value()
}
public fun queued_withdraw_shares<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.queued_withdraw_shares.value()
}
public fun share_supply<U, S, V>(vault: &Vault<U, S, V>): u64 {
    coin::total_supply(&vault.share_treasury)
}
public fun pending_positions<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.positions_tail - vault.positions_head
}
public fun round_premium_collected<U, S, V>(vault: &Vault<U, S, V>): u64 {
    vault.round_premium_collected
}

public fun pps_at<U, S, V>(vault: &Vault<U, S, V>, round: u64): u128 {
    assert!(vault.pps.contains(round), errors::vault_round_not_finalized());
    vault.pps[round]
}

public fun receipt_round(receipt: &DepositReceipt): u64 { receipt.round }
public fun receipt_amount(receipt: &DepositReceipt): u64 { receipt.amount }
public fun withdraw_receipt_round(receipt: &WithdrawReceipt): u64 { receipt.round }
public fun withdraw_receipt_shares(receipt: &WithdrawReceipt): u64 { receipt.shares }

#[test_only]
public fun underlying_feed_id_for_testing<U, S, V>(vault: &Vault<U, S, V>): vector<u8> {
    vault.config.underlying_feed_id
}

#[test_only]
public fun settlement_feed_id_for_testing<U, S, V>(vault: &Vault<U, S, V>): vector<u8> {
    vault.config.settlement_feed_id
}

// ════════════════════════════ test hooks ════════════════════════════
// PriceInfoObject cannot be forged outside the pyth package, so the
// oracle-gated cranks expose spot-injecting twins for unit tests — same
// internals, same checks past the oracle extraction.

#[test_only]
public fun finalize_round_with_spot_for_testing<U, S, V>(
    vault: &mut Vault<U, S, V>,
    treasury: &mut Treasury,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    finalize_round_internal(vault, treasury, spot, spot_scale, clock, ctx)
}

#[test_only]
public fun select_bucket_with_spot_for_testing<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
) {
    select_bucket_internal(vault, bucket, spot, spot_scale, clock)
}

#[test_only]
public fun open_rfq_with_spot_for_testing<U, S, V, C>(
    vault: &mut Vault<U, S, V>,
    bucket: &Bucket<U, S, C>,
    slice_amount: u64,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    open_rfq_internal(vault, bucket, slice_amount, spot, spot_scale, clock, ctx)
}

/// Drop settlement straight into the proceeds balance, so swap-auction
/// tests don't have to run a whole premium/exercise round to fund one.
#[test_only]
public fun inject_proceeds_for_testing<U, S, V>(vault: &mut Vault<U, S, V>, coin: Coin<S>) {
    vault.proceeds_settlement.join(coin.into_balance());
}

/// Open a proceeds-swap auction with a forged spot — same internals,
/// same reserve floor, without a live `PriceInfoObject`.
#[test_only]
public fun open_swap_rfq_with_spot_for_testing<U, S, V>(
    vault: &mut Vault<U, S, V>,
    amount_s: u64,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    open_swap_rfq_internal(vault, amount_s, spot, spot_scale, clock, ctx)
}

/// Settle a proceeds-swap auction with a forged spot: exercises the
/// fresh-Pyth band check, the fill/refund routing, and the round
/// accounting without a live `PriceInfoObject`.
#[test_only]
public fun settle_swap_rfq_with_spot_for_testing<U, S, V>(
    vault: &mut Vault<U, S, V>,
    auction: Auction<S, U>,
    spot: u128,
    spot_scale: u8,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    settle_swap_rfq_internal(vault, auction, spot, spot_scale, clock, ctx)
}
