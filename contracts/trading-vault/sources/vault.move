/// Curated trading vault (docs/trading-vault/01-contract-design.md):
/// permissionless creation, creator-picked curator, single per-vault
/// deposit asset, allowlisted-adapter sessions for deployment.
///
/// Principles:
/// 1. **Curator trades, never withdraws** — no vault function returns
///    funds to the transaction sender. Balances leave only into
///    allowlisted adapter code paths inside a `Session` hot potato, and
///    adapters by construction return outputs to the vault (`put` /
///    `put_position`). Depositor security reduces to vault-core
///    invariants plus the audit of each allowlisted adapter.
/// 2. **Ledger shares, per-user cost basis** — non-transferable stakes;
///    the curator's performance fee is charged on each user's own profit
///    at crystallization (fulfillment or closure), never before.
/// 3. **Oracle-free trading, oracle-priced accounting** — nothing
///    constrains curator orders; NAV for share pricing comes from
///    `PriceAttestation`s (allowlisted oracle adapters) via the
///    `Appraisal` hot potato, complete-and-same-transaction by
///    construction.
///
/// Accounting: u128 shares; NAV in deposit-asset smallest-units; value =
/// shares × nav / total_shares with u256 intermediates and floor
/// division (dust favors remaining depositors). The curator's fee is
/// minted as shares at the same ratio, which keeps price-per-share
/// invariant for everyone else (see the fulfillment math below).
module trading_vault::vault;

use std::type_name::{Self, TypeName};
use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::dynamic_field as df;
use sui::dynamic_object_field as dof;
use sui::table::{Self, Table};
use sui::transfer::Receiving;
use sui::vec_map::{Self, VecMap};
use sui::vec_set::{Self, VecSet};

use options_core::admin::AdminCap;
use options_core::errors as core_errors;
use options_core::treasury::{Self, Treasury};

use trading_vault::errors;
use trading_vault::events;
use trading_vault::price::{Self, PriceAttestation};
use trading_vault::registry::{Self, IntegrationRegistry, VaultProtocolConfig};

const BPS_DENOM: u128 = 10_000;

// Rotation authority (who may reassign the curator role).
const ROTATE_CREATOR: u8 = 0;
const ROTATE_CURATOR: u8 = 1;
const ROTATE_EITHER: u8 = 2;

public enum VaultState has copy, drop, store {
    Open,
    /// Unwind only: no deposits, sessions still run so the curator (or,
    /// permissionlessly, force cranks) can flatten positions.
    Closing,
    /// Terminal: everyone exits through the queue, lockups and the
    /// curator floor waived.
    Closed,
}

/// Stakes are keyed by address for depositors and by the CuratorCap id
/// for the curator role: skin-in-the-game travels with the cap. A
/// rotated-out cap keeps its stake as a pure claim ticket (whoever holds
/// the old cap can queue its withdrawal); the floor only ever binds the
/// CURRENT cap.
public enum StakeKey has copy, drop, store {
    Addr(address),
    Cap(ID),
}

public struct Stake has store {
    shares: u128,
    /// Deposit-asset smallest-units; reduced pro-rata on request.
    cost_basis: u64,
    locked_until_ms: u64,
}

public struct VaultConfig has copy, drop, store {
    deposit_asset: TypeName,
    lockup_ms: u64,
    curator_fee_bps: u64,
    rotation_authority: u8,
    /// Bounds the appraisal PTB; sessions cannot custody more.
    max_positions: u64,
    /// Queue-head age after which permissionless force-unwind sessions
    /// unlock.
    unwind_grace_ms: u64,
    deposits_paused: bool,
}

public struct WithdrawRequest has store {
    key: StakeKey,
    recipient: address,
    shares: u128,
    basis: u64,
    requested_at_ms: u64,
}

public struct TradingVault has key {
    id: UID,
    creator: address,
    curator_cap_id: ID,
    state: VaultState,
    config: VaultConfig,
    total_shares: u128,
    stakes: Table<StakeKey, Stake>,
    /// Every asset type with a non-zero free balance (balances live as
    /// dynamic fields keyed by `BalanceKey<T>`). The appraisal's
    /// completeness check walks this set.
    asset_types: VecSet<TypeName>,
    position_count: u64,
    // FIFO withdrawal queue: `queue[head..tail)`.
    queue: Table<u64, WithdrawRequest>,
    queue_head: u64,
    queue_tail: u64,
}

/// Transferable curator role. Holding the cap named by
/// `vault.curator_cap_id` is what authorizes sessions and curator-side
/// stake operations; nothing is tied to an address, so bots and
/// multisigs work and rotation never strands the role.
public struct CuratorCap has key, store {
    id: UID,
    vault_id: ID,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

public struct PositionKey has copy, drop, store { id: ID }

/// Which adapter custodied a position; only that adapter may take or
/// appraise it.
public struct PositionTagKey has copy, drop, store { id: ID }

/// Curator-operation hot potato (design doc §3): everything a session
/// takes must resolve back into the vault this same transaction. No
/// abilities.
public struct Session {
    vault_id: ID,
    adapter: TypeName,
    forced: bool,
    taken: VecMap<TypeName, u64>,
    returned: VecMap<TypeName, u64>,
    positions_added: u64,
    positions_removed: u64,
}

/// NAV hot potato (design doc §4.2): consumed by `deposit` /
/// `fulfill_withdrawals` only when every held asset type and every
/// custodied position has been valued, and nothing moved since `begin`.
public struct Appraisal {
    vault_id: ID,
    total_value: u128,
    remaining_types: VecSet<TypeName>,
    appraised_positions: VecSet<ID>,
    position_total: u64,
    /// Anything changing mid-PTB (a session between begin and consume)
    /// invalidates the snapshot and aborts at consume.
    types_snapshot: VecSet<TypeName>,
    deposit_balance_snapshot: u64,
}

// ═══════════════════════════════ creation ═══════════════════════════════

/// Permissionless. The creator picks the curator; the cap is transferred
/// to them. No seed deposit is required: with no donation path into the
/// vault, NAV cannot be inflated ahead of the first depositor, so the
/// classic share-inflation attack has no lever.
public fun create_vault<T>(
    cfg: &VaultProtocolConfig,
    curator: address,
    lockup_ms: u64,
    curator_fee_bps: u64,
    rotation_authority: u8,
    max_positions: u64,
    unwind_grace_ms: u64,
    ctx: &mut TxContext,
): ID {
    assert!(curator_fee_bps <= registry::max_curator_fee_bps(cfg), errors::fee_too_high());
    assert!(rotation_authority <= ROTATE_EITHER, errors::config_invalid());
    assert!(max_positions > 0, errors::config_invalid());

    let mut vault = TradingVault {
        id: object::new(ctx),
        creator: ctx.sender(),
        curator_cap_id: object::id_from_address(@0x0), // set below
        state: VaultState::Open,
        config: VaultConfig {
            deposit_asset: type_name::with_defining_ids<T>(),
            lockup_ms,
            curator_fee_bps,
            rotation_authority,
            max_positions,
            unwind_grace_ms,
            deposits_paused: false,
        },
        total_shares: 0,
        stakes: table::new(ctx),
        asset_types: vec_set::empty(),
        position_count: 0,
        queue: table::new(ctx),
        queue_head: 0,
        queue_tail: 0,
    };
    let vault_id = object::id(&vault);
    let cap = CuratorCap { id: object::new(ctx), vault_id };
    let cap_id = object::id(&cap);
    vault.curator_cap_id = cap_id;

    events::emit_vault_created(
        vault_id,
        vault.creator,
        curator,
        cap_id,
        vault.config.deposit_asset,
        lockup_ms,
        curator_fee_bps,
        rotation_authority,
        max_positions,
        unwind_grace_ms,
    );
    transfer::public_transfer(cap, curator);
    transfer::share_object(vault);
    vault_id
}

// ══════════════════════════ deposits and stakes ══════════════════════════

/// Deposit into an address-keyed stake. Requires a complete appraisal so
/// shares are minted at true NAV.
public fun deposit<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    funds: Coin<T>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let key = StakeKey::Addr(ctx.sender());
    deposit_internal<T>(vault, cfg, appraisal, funds, key, option::none(), clock, ctx);
}

/// Deposit into the curator's cap-keyed stake (their floor stake).
public fun deposit_as_curator<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    cap: &CuratorCap,
    appraisal: Appraisal,
    funds: Coin<T>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    let cap_id = object::id(cap);
    deposit_internal<T>(
        vault,
        cfg,
        appraisal,
        funds,
        StakeKey::Cap(cap_id),
        option::some(cap_id),
        clock,
        ctx,
    );
}

fun deposit_internal<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    funds: Coin<T>,
    key: StakeKey,
    cap_for_event: Option<ID>,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(!registry::is_paused(cfg), errors::protocol_paused());
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(!vault.config.deposits_paused, errors::deposits_paused());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.deposit_asset,
        errors::deposit_asset_mismatch(),
    );
    let amount = funds.value();
    assert!(amount > 0, core_errors::zero_amount());

    let nav = consume_appraisal<T>(vault, appraisal);
    let shares = if (vault.total_shares == 0) {
        amount as u128
    } else {
        // A wiped vault (shares outstanding, nothing left) can no longer
        // price deposits; it can only be closed out.
        assert!(nav > 0, errors::vault_dead());
        mul_div(amount as u128, vault.total_shares, nav)
    };
    assert!(shares > 0, core_errors::zero_amount());

    put_balance_internal<T>(vault, funds.into_balance());
    vault.total_shares = vault.total_shares + shares;

    let locked_until_ms = clock.timestamp_ms() + vault.config.lockup_ms;
    if (vault.stakes.contains(key)) {
        let stake = vault.stakes.borrow_mut(key);
        stake.shares = stake.shares + shares;
        stake.cost_basis = stake.cost_basis + amount;
        stake.locked_until_ms = locked_until_ms;
    } else {
        vault.stakes.add(key, Stake { shares, cost_basis: amount, locked_until_ms });
    };

    events::emit_deposited(
        object::id(vault),
        ctx.sender(),
        cap_for_event,
        amount,
        shares,
        vault.total_shares,
        locked_until_ms,
    );
}

// ═══════════════════════════ withdrawal queue ═══════════════════════════

/// Queue a withdrawal from the sender's stake. Crystallization (value,
/// profit, fees) happens at fulfillment, so queued shares keep earning
/// the vault's P&L until paid.
public fun request_withdraw(
    vault: &mut TradingVault,
    shares: u128,
    clock: &Clock,
    ctx: &TxContext,
) {
    let sender = ctx.sender();
    request_internal(
        vault,
        StakeKey::Addr(sender),
        option::none(),
        shares,
        sender,
        true,
        clock,
    );
}

/// Queue a withdrawal from a cap-keyed stake. When the cap is the
/// current curator's and the floor is enforced, the request must leave
/// the curator at or above `min_curator_share_bps` of total shares.
/// Rotated-out caps are pure claim tickets: no floor, but normal lockup.
public fun request_withdraw_as_curator(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    cap: &CuratorCap,
    shares: u128,
    recipient: address,
    clock: &Clock,
) {
    assert!(cap.vault_id == object::id(vault), errors::wrong_vault());
    let cap_id = object::id(cap);
    let key = StakeKey::Cap(cap_id);
    if (
        cap_id == vault.curator_cap_id
            && registry::enforce_curator_share(cfg)
            && vault.state == VaultState::Open
    ) {
        assert!(vault.stakes.contains(key), errors::stake_missing());
        let remaining = vault.stakes.borrow(key).shares - shares;
        // remaining / total ≥ min_bps / 10⁴ (total unchanged: queued
        // shares stay outstanding until fulfillment).
        assert!(
            (remaining as u256) * (BPS_DENOM as u256)
                >= (registry::min_curator_share_bps(cfg) as u256) * (vault.total_shares as u256),
            errors::curator_floor(),
        );
    };
    request_internal(vault, key, option::some(cap_id), shares, recipient, true, clock);
}

/// Permissionless once the vault is Closed: anyone may push a remaining
/// address stake into the queue so no depositor is stranded.
public fun enqueue_closed_stake(
    vault: &mut TradingVault,
    owner: address,
    clock: &Clock,
) {
    assert!(vault.state == VaultState::Closed, errors::vault_not_closed());
    let key = StakeKey::Addr(owner);
    let shares = {
        assert!(vault.stakes.contains(key), errors::stake_missing());
        vault.stakes.borrow(key).shares
    };
    request_internal(vault, key, option::none(), shares, owner, false, clock);
}

fun request_internal(
    vault: &mut TradingVault,
    key: StakeKey,
    cap_for_event: Option<ID>,
    shares: u128,
    recipient: address,
    check_lockup: bool,
    clock: &Clock,
) {
    assert!(shares > 0, core_errors::zero_amount());
    assert!(vault.stakes.contains(key), errors::stake_missing());
    let now = clock.timestamp_ms();
    let closed = vault.state == VaultState::Closed;

    let (basis_out, emptied) = {
        let stake = vault.stakes.borrow_mut(key);
        assert!(shares <= stake.shares, errors::insufficient_balance());
        if (check_lockup && !closed) {
            assert!(now >= stake.locked_until_ms, errors::still_locked());
        };
        // Pro-rata basis; the last slice takes the floor remainder too.
        let basis_out = if (shares == stake.shares) {
            stake.cost_basis
        } else {
            (mul_div(stake.cost_basis as u128, shares, stake.shares)) as u64
        };
        stake.shares = stake.shares - shares;
        stake.cost_basis = stake.cost_basis - basis_out;
        (basis_out, stake.shares == 0)
    };
    if (emptied) {
        let Stake { shares: _, cost_basis: _, locked_until_ms: _ } = vault.stakes.remove(key);
    };

    let seq = vault.queue_tail;
    vault.queue.add(seq, WithdrawRequest { key, recipient, shares, basis: basis_out, requested_at_ms: now });
    vault.queue_tail = seq + 1;
    events::emit_withdraw_requested(
        object::id(vault),
        seq,
        recipient,
        cap_for_event,
        shares,
        basis_out,
        now,
    );
}

/// Permissionless crank: fulfill queued requests FIFO while the free
/// deposit-asset balance covers them, crystallizing at this appraisal's
/// NAV.
///
/// Per request (all floor division):
///   value        = shares × nav / total_shares
///   profit       = max(0, value − basis)
///   gross_fee    = profit × curator_fee_bps / 10⁴
///   protocol_cut = gross_fee × protocol_fee_bps / 10⁴   (Morpho-style)
///   curator_net  = gross_fee − protocol_cut
///   payout       = value − gross_fee                    (cash out)
/// The curator's net is minted back as shares at the SAME nav/shares
/// ratio (m = curator_net × total_shares / nav), which leaves
/// price-per-share unchanged for every remaining depositor; using the
/// ratio fixed at appraisal time for the whole batch keeps the math
/// consistent across sequential requests.
public fun fulfill_withdrawals<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    appraisal: Appraisal,
    ctx: &mut TxContext,
) {
    assert!(
        type_name::with_defining_ids<T>() == vault.config.deposit_asset,
        errors::deposit_asset_mismatch(),
    );
    let nav = consume_appraisal<T>(vault, appraisal);
    let ratio_nav = nav;
    let ratio_shares = vault.total_shares;
    assert!(ratio_shares > 0 || vault.queue_head == vault.queue_tail, errors::vault_dead());

    let curator_key = StakeKey::Cap(vault.curator_cap_id);
    let protocol_fee_bps = registry::protocol_fee_bps(cfg) as u128;
    let curator_fee_bps = vault.config.curator_fee_bps as u128;
    let vault_id = object::id(vault);

    while (vault.queue_head < vault.queue_tail) {
        let seq = vault.queue_head;
        let (value, payout, protocol_cut) = {
            let req = vault.queue.borrow(seq);
            let value = (mul_div(req.shares, ratio_nav, ratio_shares)) as u64;
            let profit = if (value > req.basis) { value - req.basis } else { 0 };
            let gross_fee = ((profit as u128) * curator_fee_bps / BPS_DENOM) as u64;
            let protocol_cut = ((gross_fee as u128) * protocol_fee_bps / BPS_DENOM) as u64;
            (value, value - gross_fee, protocol_cut)
        };
        // All-or-nothing per request; stop at the first we cannot fund.
        if ((payout as u128) + (protocol_cut as u128) > (free_balance_value<T>(vault) as u128)) {
            break
        };

        let WithdrawRequest { key: _, recipient, shares, basis, requested_at_ms: _ } =
            vault.queue.remove(seq);
        vault.queue_head = seq + 1;

        let profit = if (value > basis) { value - basis } else { 0 };
        let gross_fee = ((profit as u128) * curator_fee_bps / BPS_DENOM) as u64;
        let curator_net = gross_fee - protocol_cut;

        vault.total_shares = vault.total_shares - shares;

        // Curator fee: mint shares into the current cap's stake at the
        // batch ratio; the value stays in the vault. Fee shares carry no
        // fresh lockup — the floor is the curator's binding constraint.
        let minted = if (curator_net > 0) {
            let m = mul_div(curator_net as u128, ratio_shares, ratio_nav);
            if (m > 0) {
                vault.total_shares = vault.total_shares + m;
                if (vault.stakes.contains(curator_key)) {
                    let stake = vault.stakes.borrow_mut(curator_key);
                    stake.shares = stake.shares + m;
                    stake.cost_basis = stake.cost_basis + curator_net;
                } else {
                    vault.stakes.add(
                        curator_key,
                        Stake { shares: m, cost_basis: curator_net, locked_until_ms: 0 },
                    );
                };
            };
            m
        } else { 0 };

        if (protocol_cut > 0) {
            treasury::deposit_balance(treasury, take_balance_internal<T>(vault, protocol_cut));
        };
        if (payout > 0) {
            transfer::public_transfer(
                coin::from_balance(take_balance_internal<T>(vault, payout), ctx),
                recipient,
            );
        };

        events::emit_withdraw_fulfilled(
            vault_id,
            seq,
            recipient,
            shares,
            value,
            basis,
            profit,
            gross_fee,
            protocol_cut,
            curator_net,
            minted,
            payout,
            vault.total_shares,
        );
    }
}

// ═══════════════════════════════ sessions ═══════════════════════════════

/// Open a curator session for an allowlisted adapter. Legal while Open
/// or Closing (Closing sessions are how the curator unwinds; adapters
/// gate any discretionary entry points on `is_open`).
public fun begin_session<W: drop>(
    vault: &TradingVault,
    cap: &CuratorCap,
    reg: &IntegrationRegistry,
    _witness: W,
): Session {
    assert_current_cap(vault, cap);
    assert!(vault.state != VaultState::Closed, errors::vault_not_open());
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    new_session(vault, adapter, false)
}

/// Permissionless conservative session: unlocked when the vault is
/// Closing, or when the queue head has aged past `unwind_grace_ms`.
/// Cannot `take` — only return value to the vault (cancel orders, sweep
/// venue balances, redeem expired positions).
public fun begin_force_session<W: drop>(
    vault: &TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
    clock: &Clock,
): Session {
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    let ready = vault.state == VaultState::Closing || {
        vault.queue_head < vault.queue_tail && {
            let head = vault.queue.borrow(vault.queue_head);
            clock.timestamp_ms() > head.requested_at_ms + vault.config.unwind_grace_ms
        }
    };
    assert!(ready, errors::unwind_not_ready());
    new_session(vault, adapter, true)
}

/// Permissionless, always-available session for adapter CRANKS — the
/// non-discretionary maintenance moves whose outcome is fixed by prior
/// state (settle a finished auction, redeem an expired position, sweep
/// settled venue amounts). Like a force session it can never `take`
/// balances; unlike one it has no unlock condition, so adapters must
/// expose through it only entry points that cannot grief the strategy.
public fun begin_crank_session<W: drop>(
    vault: &TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
): Session {
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    new_session(vault, adapter, true)
}

fun new_session(vault: &TradingVault, adapter: TypeName, forced: bool): Session {
    Session {
        vault_id: object::id(vault),
        adapter,
        forced,
        taken: vec_map::empty(),
        returned: vec_map::empty(),
        positions_added: 0,
        positions_removed: 0,
    }
}

/// Hand funds to the adapter code path. Never callable from a forced
/// session.
public fun take<T>(vault: &mut TradingVault, s: &mut Session, amount: u64): Balance<T> {
    assert!(s.vault_id == object::id(vault), errors::wrong_vault());
    assert!(!s.forced, errors::forced_session_take());
    assert!(amount > 0, core_errors::zero_amount());
    bump(&mut s.taken, type_name::with_defining_ids<T>(), amount);
    take_balance_internal<T>(vault, amount)
}

/// Return value to the vault's free balances.
public fun put<T>(vault: &mut TradingVault, s: &mut Session, funds: Balance<T>) {
    assert!(s.vault_id == object::id(vault), errors::wrong_vault());
    if (funds.value() == 0) {
        funds.destroy_zero();
        return
    };
    bump(&mut s.returned, type_name::with_defining_ids<T>(), funds.value());
    put_balance_internal<T>(vault, funds);
}

/// Custody a position object under this session's adapter tag.
public fun put_position<P: key + store>(vault: &mut TradingVault, s: &mut Session, p: P) {
    assert!(s.vault_id == object::id(vault), errors::wrong_vault());
    s.positions_added = s.positions_added + 1;
    store_position_internal(vault, s.adapter, p);
}

/// Remove a position this session's adapter custodied.
public fun take_position<P: key + store>(
    vault: &mut TradingVault,
    s: &mut Session,
    position_id: ID,
): P {
    assert!(s.vault_id == object::id(vault), errors::wrong_vault());
    let tag_key = PositionTagKey { id: position_id };
    assert!(df::exists(&vault.id, tag_key), errors::position_missing());
    let tag: TypeName = df::remove(&mut vault.id, tag_key);
    assert!(tag == s.adapter, errors::adapter_not_allowed());
    let p: P = dof::remove(&mut vault.id, PositionKey { id: position_id });
    vault.position_count = vault.position_count - 1;
    s.positions_removed = s.positions_removed + 1;
    events::emit_position_removed(object::id(vault), s.adapter, position_id);
    p
}

/// Sweep in a position that was transferred to the vault's own object
/// address (e.g. an RFQ settlement minting to the vault). Witness-gated
/// so junk objects can never inflate `position_count` and wedge
/// appraisals — unclaimed transfers just sit unreceived.
public fun receive_position<P: key + store, W: drop>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
    receiving: Receiving<P>,
) {
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    let p = transfer::public_receive(&mut vault.id, receiving);
    store_position_internal(vault, adapter, p);
}

/// Sweep in a Coin that was transferred to the vault's own object
/// address (e.g. RFQ premium routed to the vault). Witness-gated like
/// `receive_position`; joins straight into free balances.
public fun receive_coin<T, W: drop>(
    vault: &mut TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
    receiving: Receiving<Coin<T>>,
) {
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    let c = transfer::public_receive(&mut vault.id, receiving);
    if (c.value() == 0) {
        c.destroy_zero();
        return
    };
    put_balance_internal<T>(vault, c.into_balance());
}

public fun end_session(vault: &TradingVault, s: Session) {
    let Session { vault_id, adapter, forced, taken, returned, positions_added, positions_removed } =
        s;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
    events::emit_session_settled(
        vault_id,
        adapter,
        forced,
        taken,
        returned,
        positions_added,
        positions_removed,
    );
}

fun store_position_internal<P: key + store>(vault: &mut TradingVault, adapter: TypeName, p: P) {
    assert!(vault.position_count < vault.config.max_positions, errors::too_many_positions());
    let position_id = object::id(&p);
    df::add(&mut vault.id, PositionTagKey { id: position_id }, adapter);
    dof::add(&mut vault.id, PositionKey { id: position_id }, p);
    vault.position_count = vault.position_count + 1;
    events::emit_position_stored(object::id(vault), adapter, position_id);
}

// ══════════════════════════════ appraisal ══════════════════════════════

/// Start a NAV computation. The deposit asset values itself 1:1; every
/// other held type needs `appraise_balance`, every custodied position
/// needs its adapter's `appraise_*` to call `record_position_value`.
public fun begin_appraisal<T>(vault: &TradingVault): Appraisal {
    assert!(
        type_name::with_defining_ids<T>() == vault.config.deposit_asset,
        errors::deposit_asset_mismatch(),
    );
    let deposit_balance = free_balance_value<T>(vault);
    let mut remaining = vault.asset_types;
    if (remaining.contains(&vault.config.deposit_asset)) {
        remaining.remove(&vault.config.deposit_asset);
    };
    Appraisal {
        vault_id: object::id(vault),
        total_value: deposit_balance as u128,
        remaining_types: remaining,
        appraised_positions: vec_set::empty(),
        position_total: vault.position_count,
        types_snapshot: vault.asset_types,
        deposit_balance_snapshot: deposit_balance,
    }
}

/// Value one non-deposit free balance via an oracle attestation.
public fun appraise_balance<T>(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    a: &mut Appraisal,
    att: PriceAttestation,
    clock: &Clock,
) {
    assert!(a.vault_id == object::id(vault), errors::wrong_vault());
    let t = type_name::with_defining_ids<T>();
    assert!(a.remaining_types.contains(&t), errors::already_appraised());
    assert!(price::asset(&att) == t, errors::price_asset_mismatch());
    assert!(
        price::quote_asset(&att) == vault.config.deposit_asset,
        errors::price_asset_mismatch(),
    );
    assert_attestation_fresh(cfg, &att, clock);

    let amount = free_balance_value<T>(vault) as u128;
    a.total_value = a.total_value + mul_div(amount, price::price(&att), price::price_scale());
    a.remaining_types.remove(&t);
}

/// Record one custodied position's value, in deposit-asset units. Only
/// the adapter that custodied the position may value it (witness must
/// match the tag); how it derives the value — and which attestations it
/// needs — is the adapter's contract.
public fun record_position_value<W: drop>(
    vault: &TradingVault,
    a: &mut Appraisal,
    _witness: W,
    position_id: ID,
    value: u64,
) {
    assert!(a.vault_id == object::id(vault), errors::wrong_vault());
    let tag_key = PositionTagKey { id: position_id };
    assert!(df::exists(&vault.id, tag_key), errors::position_missing());
    let tag: &TypeName = df::borrow(&vault.id, tag_key);
    assert!(*tag == type_name::with_defining_ids<W>(), errors::adapter_not_allowed());
    assert!(!a.appraised_positions.contains(&position_id), errors::already_appraised());
    a.appraised_positions.insert(position_id);
    a.total_value = a.total_value + (value as u128);
}

/// Completeness + staleness gate, returns NAV. `T` is the deposit asset
/// (both callers assert it) so the balance snapshot can be re-read.
#[allow(lint(collection_equality))]
fun consume_appraisal<T>(vault: &TradingVault, a: Appraisal): u128 {
    let Appraisal {
        vault_id,
        total_value,
        remaining_types,
        appraised_positions,
        position_total,
        types_snapshot,
        deposit_balance_snapshot,
    } = a;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
    assert!(remaining_types.is_empty(), errors::appraisal_incomplete());
    assert!(appraised_positions.length() == position_total, errors::appraisal_incomplete());
    // Nothing may have moved since begin (same-PTB sessions invalidate).
    assert!(position_total == vault.position_count, errors::appraisal_mismatch());
    assert!(types_snapshot == vault.asset_types, errors::appraisal_mismatch());
    assert!(deposit_balance_snapshot == free_balance_value<T>(vault), errors::appraisal_mismatch());
    total_value
}

fun assert_attestation_fresh(cfg: &VaultProtocolConfig, att: &PriceAttestation, clock: &Clock) {
    let now = clock.timestamp_ms();
    let ts = price::timestamp_ms(att);
    // Future timestamps (skew) are fine.
    if (ts < now) {
        assert!(now - ts <= registry::max_price_age_ms(cfg), errors::price_stale());
    };
}

/// For adapters valuing their own holdings inside a position appraisal:
/// asserts the attestation quotes into this vault's deposit asset and is
/// fresh under the protocol backstop.
public fun check_attestation(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    att: &PriceAttestation,
    clock: &Clock,
) {
    assert!(
        price::quote_asset(att) == vault.config.deposit_asset,
        errors::price_asset_mismatch(),
    );
    assert_attestation_fresh(cfg, att, clock);
}

// ═══════════════════════ closure and rotation ═══════════════════════

public fun initiate_close(vault: &mut TradingVault, cap: &CuratorCap) {
    assert_current_cap(vault, cap);
    initiate_close_internal(vault);
}

public fun initiate_close_admin(_: &AdminCap, vault: &mut TradingVault) {
    initiate_close_internal(vault);
}

fun initiate_close_internal(vault: &mut TradingVault) {
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    vault.state = VaultState::Closing;
    events::emit_vault_closing(object::id(vault));
}

/// Permissionless: Closing → Closed once every position is gone and only
/// the deposit asset remains.
public fun finalize_close(vault: &mut TradingVault) {
    assert!(vault.state == VaultState::Closing, errors::vault_not_closing());
    assert!(vault.position_count == 0, errors::positions_open());
    let n = vault.asset_types.length();
    let clean = n == 0
        || (n == 1 && vault.asset_types.contains(&vault.config.deposit_asset));
    assert!(clean, errors::residual_assets());
    vault.state = VaultState::Closed;
    events::emit_vault_closed(object::id(vault));
}

public fun rotate_curator_by_creator(
    vault: &mut TradingVault,
    recipient: address,
    ctx: &mut TxContext,
) {
    assert!(ctx.sender() == vault.creator, errors::not_authorized());
    let auth = vault.config.rotation_authority;
    assert!(auth == ROTATE_CREATOR || auth == ROTATE_EITHER, errors::not_authorized());
    rotate_internal(vault, recipient, ctx);
}

public fun rotate_curator_by_curator(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    recipient: address,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    let auth = vault.config.rotation_authority;
    assert!(auth == ROTATE_CURATOR || auth == ROTATE_EITHER, errors::not_authorized());
    rotate_internal(vault, recipient, ctx);
}

/// Mint a fresh cap and invalidate the old one for curation. The old
/// cap's stake stays keyed by the old cap id — a claim ticket for
/// whoever holds it, no longer floor-bound.
fun rotate_internal(vault: &mut TradingVault, recipient: address, ctx: &mut TxContext) {
    let old_cap_id = vault.curator_cap_id;
    let cap = CuratorCap { id: object::new(ctx), vault_id: object::id(vault) };
    let new_cap_id = object::id(&cap);
    vault.curator_cap_id = new_cap_id;
    events::emit_curator_rotated(object::id(vault), old_cap_id, new_cap_id, recipient);
    transfer::public_transfer(cap, recipient);
}

public fun set_deposits_paused(vault: &mut TradingVault, cap: &CuratorCap, paused: bool) {
    assert_current_cap(vault, cap);
    vault.config.deposits_paused = paused;
    events::emit_deposits_paused(object::id(vault), paused);
}

// ════════════════════════════ internals ════════════════════════════

fun assert_current_cap(vault: &TradingVault, cap: &CuratorCap) {
    assert!(cap.vault_id == object::id(vault), errors::wrong_vault());
    assert!(object::id(cap) == vault.curator_cap_id, errors::not_curator());
}

fun put_balance_internal<T>(vault: &mut TradingVault, funds: Balance<T>) {
    let key = BalanceKey<T> {};
    let t = type_name::with_defining_ids<T>();
    if (df::exists(&vault.id, key)) {
        let bal: &mut Balance<T> = df::borrow_mut(&mut vault.id, key);
        bal.join(funds);
    } else {
        df::add(&mut vault.id, key, funds);
    };
    if (!vault.asset_types.contains(&t)) {
        vault.asset_types.insert(t);
    };
}

fun take_balance_internal<T>(vault: &mut TradingVault, amount: u64): Balance<T> {
    let key = BalanceKey<T> {};
    assert!(df::exists(&vault.id, key), errors::insufficient_balance());
    let out = {
        let bal: &mut Balance<T> = df::borrow_mut(&mut vault.id, key);
        assert!(bal.value() >= amount, errors::insufficient_balance());
        bal.split(amount)
    };
    let remaining: &Balance<T> = df::borrow(&vault.id, key);
    if (remaining.value() == 0) {
        let zero: Balance<T> = df::remove(&mut vault.id, key);
        zero.destroy_zero();
        vault.asset_types.remove(&type_name::with_defining_ids<T>());
    };
    out
}

/// Package-private outflow for the first-party `vault_mm` module: pulls
/// quote-collateral without a session. Every caller must have already
/// verified a core-minted `CollateralRequest` naming this vault as
/// `collateral_source` and the vault as the output recipient — see
/// `vault_mm.move` for the full authorization chain.
public(package) fun release_for_mm<T>(vault: &mut TradingVault, amount: u64): Balance<T> {
    take_balance_internal<T>(vault, amount)
}

fun free_balance_value<T>(vault: &TradingVault): u64 {
    let key = BalanceKey<T> {};
    if (!df::exists(&vault.id, key)) {
        return 0
    };
    let bal: &Balance<T> = df::borrow(&vault.id, key);
    bal.value()
}

fun bump(map: &mut VecMap<TypeName, u64>, t: TypeName, amount: u64) {
    if (map.contains(&t)) {
        let v = map.get_mut(&t);
        *v = *v + amount;
    } else {
        map.insert(t, amount);
    };
}

fun mul_div(a: u128, b: u128, c: u128): u128 {
    assert!(c > 0, core_errors::zero_amount());
    (((a as u256) * (b as u256)) / (c as u256)) as u128
}

// ══════════════════════════════ getters ══════════════════════════════

public fun is_open(vault: &TradingVault): bool { vault.state == VaultState::Open }

public fun is_closing(vault: &TradingVault): bool { vault.state == VaultState::Closing }

public fun is_closed(vault: &TradingVault): bool { vault.state == VaultState::Closed }

public fun deposit_asset(vault: &TradingVault): TypeName { vault.config.deposit_asset }

public fun total_shares(vault: &TradingVault): u128 { vault.total_shares }

public fun position_count(vault: &TradingVault): u64 { vault.position_count }

public fun free_balance_of<T>(vault: &TradingVault): u64 { free_balance_value<T>(vault) }

public fun curator_cap_id(vault: &TradingVault): ID { vault.curator_cap_id }

public fun creator(vault: &TradingVault): address { vault.creator }

public fun lockup_ms(vault: &TradingVault): u64 { vault.config.lockup_ms }

public fun curator_fee_bps(vault: &TradingVault): u64 { vault.config.curator_fee_bps }

public fun max_positions(vault: &TradingVault): u64 { vault.config.max_positions }

public fun unwind_grace_ms(vault: &TradingVault): u64 { vault.config.unwind_grace_ms }

public fun pending_withdrawals(vault: &TradingVault): u64 {
    vault.queue_tail - vault.queue_head
}

public fun stake_of(vault: &TradingVault, owner: address): (u128, u64, u64) {
    stake_fields(vault, StakeKey::Addr(owner))
}

public fun curator_stake_of(vault: &TradingVault, cap_id: ID): (u128, u64, u64) {
    stake_fields(vault, StakeKey::Cap(cap_id))
}

fun stake_fields(vault: &TradingVault, key: StakeKey): (u128, u64, u64) {
    if (!vault.stakes.contains(key)) {
        return (0, 0, 0)
    };
    let s = vault.stakes.borrow(key);
    (s.shares, s.cost_basis, s.locked_until_ms)
}

/// Immutable access to a custodied position (e.g. for appraisal reads).
/// Mutation still requires a session's `take_position`/`put_position`.
public fun borrow_position<P: key + store>(vault: &TradingVault, position_id: ID): &P {
    assert!(df::exists(&vault.id, PositionTagKey { id: position_id }), errors::position_missing());
    dof::borrow(&vault.id, PositionKey { id: position_id })
}

public fun has_position(vault: &TradingVault, position_id: ID): bool {
    df::exists(&vault.id, PositionTagKey { id: position_id })
}

public fun position_adapter(vault: &TradingVault, position_id: ID): TypeName {
    assert!(df::exists(&vault.id, PositionTagKey { id: position_id }), errors::position_missing());
    *df::borrow(&vault.id, PositionTagKey { id: position_id })
}

public fun session_adapter(s: &Session): TypeName { s.adapter }

public fun session_is_forced(s: &Session): bool { s.forced }

public fun session_vault_id(s: &Session): ID { s.vault_id }

public fun appraisal_value(a: &Appraisal): u128 { a.total_value }
