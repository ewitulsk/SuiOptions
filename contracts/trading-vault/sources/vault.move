/// Curated trading vault (docs/trading-vault/01-contract-design.md):
/// permissionless creation, creator starts as curator, per-vault
/// accounting asset with an allowlist of deposit/payout assets
/// (SO-370), allowlisted-adapter sessions for deployment.
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
/// Accounting: u128 shares; NAV in accounting-asset smallest-units.
/// Share math carries an OpenZeppelin-style virtual offset
/// (`SHARE_OFFSET` virtual shares against 1 virtual asset unit):
///   shares = value × (S + O) / (nav + 1)
///   value  = shares × (nav + 1) / (S + O)
/// with u256 intermediates and floor division (dust favors remaining
/// depositors). The offset makes donation-driven share inflation
/// unprofitable — a donor's NAV bump accrues overwhelmingly to shares
/// nobody owns. The curator's fee is minted as shares at the same batch
/// ratio, which keeps price-per-share invariant for everyone else (see
/// the fulfillment math below).
///
/// Multi-asset deposits/withdrawals (SO-370, dHEDGE/GLP model): the
/// `deposit_assets` allowlist names the coin types users may move in and
/// out. Non-accounting deposits are valued into the accounting asset by
/// a fresh `PriceAttestation` at entry; withdrawal requests name a
/// payout asset and are converted at fulfillment-batch prices. All
/// internal accounting (NAV, cost basis, fees, external budgets) stays
/// in accounting-asset units.
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
use trading_vault::registry::{Self, IntegrationRegistry, OracleRegistry, VaultProtocolConfig};

const BPS_DENOM: u128 = 10_000;

/// Virtual shares standing against 1 virtual accounting-asset unit in
/// every share↔value conversion (OZ ERC-4626 decimals-offset pattern).
const SHARE_OFFSET: u128 = 1_000_000;

/// Hard cap on the curator-set entry/exit haircuts.
const MAX_HAIRCUT_BPS: u64 = 500;

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
    /// Unit of account: NAV, cost basis, fees and external budgets are
    /// denominated in this asset's smallest units. Fixed at creation.
    accounting_asset: TypeName,
    /// Assets users may deposit and request payouts in. Always contains
    /// `accounting_asset`; curator-managed, capped by
    /// `VaultProtocolConfig.max_deposit_assets`.
    deposit_assets: VecSet<TypeName>,
    lockup_ms: u64,
    curator_fee_bps: u64,
    /// Queue-head age after which permissionless force-unwind sessions
    /// unlock.
    unwind_grace_ms: u64,
    /// Oracle-arb dampers on non-accounting flows: deposits credit
    /// `value × (1 − entry)/10⁴`; payouts convert at a price inflated by
    /// `exit` bps (fewer units out). Both default 0; capped at
    /// `MAX_HAIRCUT_BPS`.
    entry_haircut_bps: u64,
    exit_haircut_bps: u64,
    /// Adapter witnesses the curator has opted into for QUOTE sessions
    /// (SO-372) — permissionless, take-capable settlement sessions.
    /// Default empty; the venue-general sibling of `mm_release_enabled`.
    quote_adapters: VecSet<TypeName>,
    deposits_paused: bool,
    /// Opt-in for the `vault_mm` release path: signed quotes from the
    /// curator's bot may draw vault collateral. Off by default.
    mm_release_enabled: bool,
}

public struct WithdrawRequest has store {
    key: StakeKey,
    recipient: address,
    shares: u128,
    basis: u64,
    /// Asset the recipient asked to be paid in (∈ `deposit_assets` at
    /// request time). After `unwind_grace_ms` the crank may pay the
    /// accounting asset instead — the queue-liveness backstop.
    payout_asset: TypeName,
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
    /// Bumped by every free-balance mutation (`put_balance_internal` /
    /// `take_balance_internal`). Appraisals snapshot it at begin and
    /// abort at consume if it moved — a type-free, strictly stronger
    /// replacement for re-reading the accounting balance.
    mutation_seq: u64,
    // FIFO withdrawal queue: `queue[head..tail)`.
    queue: Table<u64, WithdrawRequest>,
    queue_head: u64,
    queue_tail: u64,
    /// Optional registered external account (see the external-account
    /// section below). `None` for vaults without one.
    external: Option<ExternalAccount>,
}

/// Capital deployed to a venue the vault cannot custody at the Move level
/// (a perps account, a margin manager, …) — a jointly-controlled address
/// registered by the protocol admin. Strategy-neutral: what the account
/// does (hedge, basis, carry) is the curator's business; the vault
/// enforces WHERE funds may go and HOW MUCH.
///
/// Move-enforced guarantees:
///   • releases only ever pay the registered address (`release_external`),
///     curator-gated, capped at `budget_bps` of appraised NAV and
///     rate-limited to `daily_release_bps` of NAV per 24h window;
///   • while exposure is live, the account's value enters every
///     appraisal through an equity attestation from the PINNED,
///     allowlisted `equity_oracle` witness — an appraisal is incomplete
///     without it (at zero exposure the leg is neither needed nor
///     accepted; see `begin_appraisal`);
///   • returns are accepted only from the registered address itself and
///     reduce `exposure`.
/// What signatures cannot prevent (adversarial trading at the venue) is
/// bounded by the budget and detected by off-chain reconciliation of
/// `exposure` vs attested equity.
public struct ExternalAccount has store {
    account: address,
    /// Oracle-adapter witness whose equity attestations value the account.
    equity_oracle: TypeName,
    /// Max total exposure, in bps of NAV at release time.
    budget_bps: u64,
    /// Max released per 24h window, in bps of NAV at release time.
    daily_release_bps: u64,
    /// released − returned, in deposit-asset units (cost, floored at 0).
    exposure: u64,
    released_in_window: u64,
    window_start_ms: u64,
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
    mutation_seq_snapshot: u64,
    /// True while an external account with LIVE exposure still needs its
    /// equity leg (`record_external_equity`). False — leg neither needed
    /// nor accepted — for vaults with no account and for accounts with
    /// zero exposure; see `begin_appraisal`.
    external_pending: bool,
}

/// Rolling window for the external-account release rate limit.
const RELEASE_WINDOW_MS: u64 = 86_400_000;

// Curator self-serve (attested) registration limits — deliberately far
// below the admin path's, which stays uncapped: an attestation proves
// only WHO holds the account, not that its budget was reviewed.
const ATTESTED_MAX_BUDGET_BPS: u64 = 2_000;
const ATTESTED_MAX_DAILY_RELEASE_BPS: u64 = 1_000;

/// Domain separator for registrar attestations (18 bytes).
const EXTERNAL_REG_DOMAIN: vector<u8> = b"tv_external_reg_v1";

// ═══════════════════════════════ creation ═══════════════════════════════

/// Permissionless. The creator IS the initial curator: the cap is
/// transferred to the sender, who can hand the role on by transferring
/// the cap or via `rotate_curator_by_curator`. No seed deposit is
/// required: with no donation path into the vault, NAV cannot be
/// inflated ahead of the first depositor, so the classic share-inflation
/// attack has no lever.
public fun create_vault<T>(
    cfg: &VaultProtocolConfig,
    lockup_ms: u64,
    curator_fee_bps: u64,
    unwind_grace_ms: u64,
    ctx: &mut TxContext,
): ID {
    assert!(curator_fee_bps <= registry::max_curator_fee_bps(cfg), errors::fee_too_high());

    let accounting = type_name::with_defining_ids<T>();
    let mut deposit_assets = vec_set::empty();
    deposit_assets.insert(accounting);
    let mut vault = TradingVault {
        id: object::new(ctx),
        creator: ctx.sender(),
        curator_cap_id: object::id_from_address(@0x0), // set below
        state: VaultState::Open,
        config: VaultConfig {
            accounting_asset: accounting,
            deposit_assets,
            lockup_ms,
            curator_fee_bps,
            unwind_grace_ms,
            entry_haircut_bps: 0,
            exit_haircut_bps: 0,
            quote_adapters: vec_set::empty(),
            deposits_paused: false,
            mm_release_enabled: false,
        },
        total_shares: 0,
        stakes: table::new(ctx),
        asset_types: vec_set::empty(),
        position_count: 0,
        mutation_seq: 0,
        queue: table::new(ctx),
        queue_head: 0,
        queue_tail: 0,
        external: option::none(),
    };
    let vault_id = object::id(&vault);
    let cap = CuratorCap { id: object::new(ctx), vault_id };
    let cap_id = object::id(&cap);
    vault.curator_cap_id = cap_id;

    events::emit_vault_created(
        vault_id,
        vault.creator,
        cap_id,
        vault.config.accounting_asset,
        lockup_ms,
        curator_fee_bps,
        unwind_grace_ms,
    );
    transfer::public_transfer(cap, ctx.sender());
    transfer::share_object(vault);
    vault_id
}

// ══════════════════════════ deposits and stakes ══════════════════════════

/// Deposit into an address-keyed stake. Requires a complete appraisal so
/// shares are minted at true NAV. `T` must be on the vault's
/// `deposit_assets` allowlist; non-accounting deposits carry a fresh
/// `PriceAttestation` (asset `T` → accounting asset) that values them,
/// accounting-asset deposits pass `none`.
public fun deposit<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    funds: Coin<T>,
    att: Option<PriceAttestation>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let key = StakeKey::Addr(ctx.sender());
    deposit_internal<T>(vault, cfg, appraisal, funds, att, key, option::none(), clock, ctx);
}

/// Deposit into the curator's cap-keyed stake (their floor stake).
public fun deposit_as_curator<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    cap: &CuratorCap,
    appraisal: Appraisal,
    funds: Coin<T>,
    att: Option<PriceAttestation>,
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
        att,
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
    att: Option<PriceAttestation>,
    key: StakeKey,
    cap_for_event: Option<ID>,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(!registry::is_paused(cfg), errors::protocol_paused());
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(!vault.config.deposits_paused, errors::deposits_paused());
    let t = type_name::with_defining_ids<T>();
    assert!(vault.config.deposit_assets.contains(&t), errors::asset_not_allowed());
    let amount = funds.value();
    assert!(amount > 0, core_errors::zero_amount());

    // Value the deposit in accounting-asset units. The attestation is
    // validated exactly like an appraisal leg (asset/quote pinning +
    // freshness); the entry haircut is the conservative-marks damper on
    // oracle error.
    let value = if (t == vault.config.accounting_asset) {
        assert!(att.is_none(), errors::price_asset_mismatch());
        att.destroy_none();
        amount
    } else {
        assert!(att.is_some(), errors::attestation_missing());
        let a = att.destroy_some();
        assert!(price::asset(&a) == t, errors::price_asset_mismatch());
        assert!(
            price::quote_asset(&a) == vault.config.accounting_asset,
            errors::price_asset_mismatch(),
        );
        assert_attestation_fresh(cfg, &a, clock);
        let gross = mul_div(amount as u128, price::price(&a), price::price_scale());
        let net = gross * (BPS_DENOM - (vault.config.entry_haircut_bps as u128)) / BPS_DENOM;
        assert!(net > 0, core_errors::zero_amount());
        net as u64
    };

    let nav = consume_appraisal(vault, appraisal);
    // A wiped vault (shares outstanding, nothing left) can no longer
    // price deposits; it can only be closed out.
    assert!(vault.total_shares == 0 || nav > 0, errors::vault_dead());
    let shares = mul_div(value as u128, vault.total_shares + SHARE_OFFSET, nav + 1);
    assert!(shares > 0, core_errors::zero_amount());

    put_balance_internal<T>(vault, funds.into_balance());
    vault.total_shares = vault.total_shares + shares;

    let locked_until_ms = clock.timestamp_ms() + vault.config.lockup_ms;
    if (vault.stakes.contains(key)) {
        let stake = vault.stakes.borrow_mut(key);
        stake.shares = stake.shares + shares;
        stake.cost_basis = stake.cost_basis + value;
        stake.locked_until_ms = locked_until_ms;
    } else {
        vault.stakes.add(key, Stake { shares, cost_basis: value, locked_until_ms });
    };

    events::emit_deposited(
        object::id(vault),
        ctx.sender(),
        cap_for_event,
        t,
        amount,
        value,
        shares,
        vault.total_shares,
        locked_until_ms,
    );
}

// ═══════════════════════ deposit-asset allowlist ═══════════════════════

/// Curator-gated: allow `T` for deposits and payout requests. Capped by
/// the protocol's `max_deposit_assets` — every allowlisted asset the
/// vault holds is a mandatory appraisal leg on every deposit and
/// fulfillment, so the list must stay small. Depositability is
/// oracle-coverage self-gating: an asset no allowlisted oracle can price
/// into the accounting asset cannot mint the attestation its deposit
/// requires.
public fun add_deposit_asset<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    cfg: &VaultProtocolConfig,
) {
    assert_current_cap(vault, cap);
    let t = type_name::with_defining_ids<T>();
    assert!(!vault.config.deposit_assets.contains(&t), errors::config_invalid());
    assert!(
        vault.config.deposit_assets.length() < registry::max_deposit_assets(cfg),
        errors::config_invalid(),
    );
    vault.config.deposit_assets.insert(t);
    events::emit_deposit_asset_added(object::id(vault), t);
}

/// Curator-gated: stop accepting `T`. Never the accounting asset; held
/// balances and pending requests in `T` are unaffected (pending payout
/// requests still settle in `T` — delisting gates new requests, not
/// exits).
public fun remove_deposit_asset<T>(vault: &mut TradingVault, cap: &CuratorCap) {
    assert_current_cap(vault, cap);
    let t = type_name::with_defining_ids<T>();
    assert!(t != vault.config.accounting_asset, errors::config_invalid());
    assert!(vault.config.deposit_assets.contains(&t), errors::config_invalid());
    vault.config.deposit_assets.remove(&t);
    events::emit_deposit_asset_removed(object::id(vault), t);
}

/// Curator-gated oracle-arb dampers on non-accounting deposits/payouts.
public fun set_haircuts(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    entry_bps: u64,
    exit_bps: u64,
) {
    assert_current_cap(vault, cap);
    assert!(entry_bps <= MAX_HAIRCUT_BPS && exit_bps <= MAX_HAIRCUT_BPS, errors::config_invalid());
    vault.config.entry_haircut_bps = entry_bps;
    vault.config.exit_haircut_bps = exit_bps;
    events::emit_haircuts_set(object::id(vault), entry_bps, exit_bps);
}

// ═══════════════════════════ withdrawal queue ═══════════════════════════

/// Queue a withdrawal from the sender's stake, payable in `P` (any
/// allowlisted asset). Crystallization (value, profit, fees) happens at
/// fulfillment, so queued shares keep earning the vault's P&L until
/// paid.
public fun request_withdraw<P>(
    vault: &mut TradingVault,
    shares: u128,
    clock: &Clock,
    ctx: &TxContext,
) {
    let sender = ctx.sender();
    let payout = assert_payout_asset<P>(vault);
    request_internal(
        vault,
        StakeKey::Addr(sender),
        option::none(),
        shares,
        sender,
        payout,
        true,
        clock,
    );
}

/// Queue a withdrawal from a cap-keyed stake. When the cap is the
/// current curator's and the floor is enforced, the request must leave
/// the curator at or above `min_curator_share_bps` of total shares.
/// Rotated-out caps are pure claim tickets: no floor, but normal lockup.
public fun request_withdraw_as_curator<P>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    cap: &CuratorCap,
    shares: u128,
    recipient: address,
    clock: &Clock,
) {
    assert!(cap.vault_id == object::id(vault), errors::wrong_vault());
    let payout = assert_payout_asset<P>(vault);
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
    request_internal(vault, key, option::some(cap_id), shares, recipient, payout, true, clock);
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
    // Enqueued on the owner's behalf — they never chose a payout asset,
    // so the accounting asset is the only defensible default.
    let payout = vault.config.accounting_asset;
    request_internal(vault, key, option::none(), shares, owner, payout, false, clock);
}

/// Re-point a pending request's payout asset (recipient-only). The
/// unwedge lever when the vault cannot source the originally requested
/// asset: the requester amends to the accounting asset (always
/// allowlisted) or any other allowlisted asset.
public fun amend_payout_asset<P>(vault: &mut TradingVault, seq: u64, ctx: &TxContext) {
    assert!(
        vault.queue_head <= seq && seq < vault.queue_tail && vault.queue.contains(seq),
        errors::request_missing(),
    );
    let payout = assert_payout_asset<P>(vault);
    let req = vault.queue.borrow_mut(seq);
    assert!(req.recipient == ctx.sender(), errors::not_authorized());
    req.payout_asset = payout;
    events::emit_payout_asset_amended(object::id(vault), seq, payout);
}

fun assert_payout_asset<P>(vault: &TradingVault): TypeName {
    let p = type_name::with_defining_ids<P>();
    assert!(vault.config.deposit_assets.contains(&p), errors::asset_not_allowed());
    p
}

fun request_internal(
    vault: &mut TradingVault,
    key: StakeKey,
    cap_for_event: Option<ID>,
    shares: u128,
    recipient: address,
    payout_asset: TypeName,
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
    vault.queue.add(
        seq,
        WithdrawRequest { key, recipient, shares, basis: basis_out, payout_asset, requested_at_ms: now },
    );
    vault.queue_tail = seq + 1;
    events::emit_withdraw_requested(
        object::id(vault),
        seq,
        recipient,
        cap_for_event,
        shares,
        basis_out,
        payout_asset,
        now,
    );
}

/// Fulfillment hot potato: one appraisal's NAV ratio plus the batch's
/// asset prices, spent across typed `fulfill_next<P>` calls so a single
/// crank transaction can pay a FIFO run of heterogeneous payout assets.
/// No abilities — `end_fulfillment` must close it in-transaction.
public struct Fulfillment {
    vault_id: ID,
    ratio_nav: u128,
    ratio_shares: u128,
    /// Payout-asset → accounting-asset price (PRICE_SCALE fixed point),
    /// locked for the whole batch. The accounting asset itself is
    /// implicit at exactly PRICE_SCALE.
    prices: VecMap<TypeName, u128>,
}

/// Open a fulfillment batch: consumes a complete appraisal, locks the
/// crystallization ratio, and validates one attestation per payout
/// asset the batch will touch (each must quote into the accounting
/// asset and be fresh).
public fun begin_fulfillment(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    atts: vector<PriceAttestation>,
    clock: &Clock,
): Fulfillment {
    let nav = consume_appraisal(vault, appraisal);
    assert!(vault.total_shares > 0 || vault.queue_head == vault.queue_tail, errors::vault_dead());

    let mut prices = vec_map::empty();
    let mut i = 0;
    while (i < atts.length()) {
        let a = atts[i];
        assert!(
            price::quote_asset(&a) == vault.config.accounting_asset,
            errors::price_asset_mismatch(),
        );
        assert_attestation_fresh(cfg, &a, clock);
        prices.insert(price::asset(&a), price::price(&a));
        i = i + 1;
    };

    Fulfillment {
        vault_id: object::id(vault),
        ratio_nav: nav,
        ratio_shares: vault.total_shares,
        prices,
    }
}

/// Permissionless crank: fulfill the queue head if it is payable in `P`,
/// crystallizing at the batch ratio.
///
/// Per request (all floor division, at the offset-adjusted ratio):
///   value        = shares × (nav + 1) / (total_shares + OFFSET)
///   profit       = max(0, value − basis)
///   gross_fee    = profit × curator_fee_bps / 10⁴
///   protocol_cut = gross_fee × protocol_fee_bps / 10⁴   (Morpho-style)
///   curator_net  = gross_fee − protocol_cut
///   payout       = value − gross_fee                    (accounting units)
/// The payout and the protocol cut convert to `P` units at the batch
/// price (exit haircut inflates the price ⇒ fewer units out) and pay
/// all-or-nothing from the free `P` balance. The curator's net is minted
/// back as shares at the SAME ratio (m = net × (S + O) / (nav + 1)),
/// which leaves price-per-share unchanged for every remaining depositor.
///
/// Returns false — a no-op, so speculative PTB chains stay safe — when
/// the queue is empty, the head is not payable in `P` (wrong asset and
/// no grace fallback), or the free `P` balance cannot fund it. The
/// grace fallback: once the head has aged past `unwind_grace_ms`, it
/// may be paid in the ACCOUNTING asset regardless of its requested
/// payout — the queue-liveness backstop behind an absent requester.
public fun fulfill_next<P>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    f: &mut Fulfillment,
    clock: &Clock,
    ctx: &mut TxContext,
): bool {
    assert!(f.vault_id == object::id(vault), errors::wrong_vault());
    if (vault.queue_head == vault.queue_tail) {
        return false
    };
    let p = type_name::with_defining_ids<P>();
    let is_accounting = p == vault.config.accounting_asset;
    let seq = vault.queue_head;

    let (value, payout_n, gross_fee, protocol_cut) = {
        let req = vault.queue.borrow(seq);
        let payable = req.payout_asset == p
            || (
                is_accounting
                    && clock.timestamp_ms() > req.requested_at_ms + vault.config.unwind_grace_ms
            );
        if (!payable) {
            return false
        };
        let value = (mul_div(req.shares, f.ratio_nav + 1, f.ratio_shares + SHARE_OFFSET)) as u64;
        let profit = if (value > req.basis) { value - req.basis } else { 0 };
        let gross_fee =
            ((profit as u128) * (vault.config.curator_fee_bps as u128) / BPS_DENOM) as u64;
        let protocol_cut =
            ((gross_fee as u128) * (registry::protocol_fee_bps(cfg) as u128) / BPS_DENOM) as u64;
        (value, value - gross_fee, gross_fee, protocol_cut)
    };

    // Convert accounting-unit obligations into `P` units at the batch
    // price. The exit haircut inflates the effective price so the
    // recipient receives slightly fewer units — the mirror of the entry
    // damper, floor dust favoring the vault throughout.
    let (payout_units, cut_units, price_used) = if (is_accounting) {
        (payout_n, protocol_cut, price::price_scale())
    } else {
        assert!(f.prices.contains(&p), errors::attestation_missing());
        let base = *f.prices.get(&p);
        let eff = base * (BPS_DENOM + (vault.config.exit_haircut_bps as u128)) / BPS_DENOM;
        (
            (mul_div(payout_n as u128, price::price_scale(), eff)) as u64,
            (mul_div(protocol_cut as u128, price::price_scale(), eff)) as u64,
            base,
        )
    };

    // All-or-nothing per request; the caller stops at the first head it
    // cannot fund.
    if ((payout_units as u128) + (cut_units as u128) > (free_balance_value<P>(vault) as u128)) {
        return false
    };

    let WithdrawRequest { key: _, recipient, shares, basis, payout_asset: _, requested_at_ms: _ } =
        vault.queue.remove(seq);
    vault.queue_head = seq + 1;
    let curator_net = gross_fee - protocol_cut;
    let profit = if (value > basis) { value - basis } else { 0 };

    vault.total_shares = vault.total_shares - shares;

    // Curator fee: mint shares into the current cap's stake at the
    // batch ratio; the value stays in the vault. Fee shares carry no
    // fresh lockup — the floor is the curator's binding constraint.
    let curator_key = StakeKey::Cap(vault.curator_cap_id);
    let minted = if (curator_net > 0) {
        let m = mul_div(curator_net as u128, f.ratio_shares + SHARE_OFFSET, f.ratio_nav + 1);
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

    if (cut_units > 0) {
        treasury::deposit_balance(treasury, take_balance_internal<P>(vault, cut_units));
    };
    if (payout_units > 0) {
        transfer::public_transfer(
            coin::from_balance(take_balance_internal<P>(vault, payout_units), ctx),
            recipient,
        );
    };

    events::emit_withdraw_fulfilled(
        object::id(vault),
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
        payout_n,
        p,
        payout_units,
        price_used,
        vault.total_shares,
    );
    true
}

public fun end_fulfillment(vault: &TradingVault, f: Fulfillment) {
    let Fulfillment { vault_id, ratio_nav: _, ratio_shares: _, prices: _ } = f;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
}

/// Convenience batch crank for the common all-accounting case: fulfill
/// consecutive heads payable in the accounting asset (including aged
/// heads via the grace fallback) until one can't be paid.
public fun fulfill_withdrawals<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    appraisal: Appraisal,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let mut f = begin_fulfillment(vault, cfg, appraisal, vector[], clock);
    while (fulfill_next<T>(vault, cfg, treasury, &mut f, clock, ctx)) {};
    end_fulfillment(vault, f);
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
///
/// Deliberately NOT allowlist-gated: delisting an adapter must stop new
/// deployment, never the exit path for value already deployed under it.
/// A forced session can only move value INTO the vault, so running one
/// against a delisted adapter cannot extend the protocol's exposure to
/// it — while requiring the allowlist would let a kill switch strand the
/// positions it was flipped to contain. `_reg` is kept in the signature
/// so existing PTB composers are unaffected.
#[allow(lint(unused_object_with_fields))]
public fun begin_force_session<W: drop>(
    vault: &TradingVault,
    _reg: &IntegrationRegistry,
    _witness: W,
    clock: &Clock,
): Session {
    let adapter = type_name::with_defining_ids<W>();
    let ready = vault.state == VaultState::Closing || {
        vault.queue_head < vault.queue_tail && {
            let head = vault.queue.borrow(vault.queue_head);
            clock.timestamp_ms() > head.requested_at_ms + vault.config.unwind_grace_ms
        }
    };
    assert!(ready, errors::unwind_not_ready());
    new_session(vault, adapter, true)
}

/// Permissionless, take-capable QUOTE session (SO-372) — the settlement
/// path for adapters that let third parties (takers, the relayer) fill
/// the vault's signed maker quotes directly against vault free balances.
/// Triple-gated: the adapter must be protocol-allowlisted AND
/// curator-opted-in via `quote_adapters` AND the vault Open.
///
/// Trust model (the vault_mm precedent, generalized): a take through
/// this session is safe only because the adapter verifies a
/// curator-authorized signed instruction — for the exchange, a
/// `FillObligation` minted by settlement after validating an order
/// signed by a curator-delegated key — and routes all value back into
/// the vault in the same transaction. That verification is a standing
/// requirement of the ADAPTER's audit; enabling a quote adapter is the
/// curator accepting that audit surface. Either kill switch (protocol
/// delist, curator remove) stops new quote sessions instantly; resting
/// orders die with the adapter's own cancel machinery.
public fun begin_quote_session<W: drop>(
    vault: &TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
): Session {
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    let adapter = type_name::with_defining_ids<W>();
    assert!(registry::is_adapter_allowed(reg, &adapter), errors::adapter_not_allowed());
    assert!(
        vault.config.quote_adapters.contains(&adapter),
        errors::quote_adapter_not_enabled(),
    );
    new_session(vault, adapter, false)
}

/// Curator opt-in for quote sessions by adapter witness `W`.
public fun add_quote_adapter<W>(vault: &mut TradingVault, cap: &CuratorCap) {
    assert_current_cap(vault, cap);
    let w = type_name::with_defining_ids<W>();
    assert!(!vault.config.quote_adapters.contains(&w), errors::config_invalid());
    vault.config.quote_adapters.insert(w);
    events::emit_quote_adapter_added(object::id(vault), w);
}

public fun remove_quote_adapter<W>(vault: &mut TradingVault, cap: &CuratorCap) {
    assert_current_cap(vault, cap);
    let w = type_name::with_defining_ids<W>();
    assert!(vault.config.quote_adapters.contains(&w), errors::config_invalid());
    vault.config.quote_adapters.remove(&w);
    events::emit_quote_adapter_removed(object::id(vault), w);
}

/// Permissionless, always-available session for adapter CRANKS — the
/// non-discretionary maintenance moves whose outcome is fixed by prior
/// state (settle a finished auction, redeem an expired position, sweep
/// settled venue amounts). Like a force session it can never `take`
/// balances; unlike one it has no unlock condition, so adapters must
/// expose through it only entry points that cannot grief the strategy.
/// Not allowlist-gated, for the same reason as `begin_force_session`: a
/// take-less maintenance crank is how value under a delisted adapter
/// gets back to depositors.
#[allow(lint(unused_object_with_fields))]
public fun begin_crank_session<W: drop>(
    vault: &TradingVault,
    _reg: &IntegrationRegistry,
    _witness: W,
): Session {
    new_session(vault, type_name::with_defining_ids<W>(), true)
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
///
/// Unlike the session kill switches this KEEPS its allowlist check: the
/// gate here is what prevents stranding, not what causes it. An
/// unreceived object is inert — it is not in `position_count` or
/// `asset_types`, so it blocks nothing; whereas an ungated sweep lets
/// anyone push an object the vault has no allowlisted appraiser for,
/// after which every appraisal (and therefore every exit) is wedged
/// permanently. An in-flight transfer to a since-delisted adapter is
/// recovered by re-allowlisting it long enough to sweep — a governance
/// act on funds that were never in custody.
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
///
/// The external-account equity leg is required only while exposure is
/// LIVE (`external.is_some() && exposure > 0`), not merely because an
/// account is registered. Before any `release_external` the account's
/// equity attributable to the vault is zero by construction — a
/// chain-verifiable fact that needs no attestation — so demanding one
/// would make deposits and exits depend on an equity poster being alive
/// for a vault that has never sent a unit out. Once every released unit
/// is back (`exposure == 0`) venue profit may still sit at the venue,
/// uncounted: that is the codebase's standard conservative-marks posture
/// (undercount, never overcount), and it self-heals on the next poster
/// update after the next release.
public fun begin_appraisal<T>(vault: &TradingVault): Appraisal {
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let accounting_balance = free_balance_value<T>(vault);
    let mut remaining = vault.asset_types;
    if (remaining.contains(&vault.config.accounting_asset)) {
        remaining.remove(&vault.config.accounting_asset);
    };
    Appraisal {
        vault_id: object::id(vault),
        total_value: accounting_balance as u128,
        remaining_types: remaining,
        appraised_positions: vec_set::empty(),
        position_total: vault.position_count,
        types_snapshot: vault.asset_types,
        mutation_seq_snapshot: vault.mutation_seq,
        external_pending: vault.external.is_some() && vault.external.borrow().exposure > 0,
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
        price::quote_asset(&att) == vault.config.accounting_asset,
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
    events::emit_position_appraised(a.vault_id, *tag, position_id, value);
}

/// Completeness + staleness gate, returns NAV. Type-free: mid-PTB
/// movement is detected by the `mutation_seq` snapshot (any free-balance
/// mutation since begin bumps it), plus the type-set and position-count
/// checks.
#[allow(lint(collection_equality))]
fun consume_appraisal(vault: &TradingVault, a: Appraisal): u128 {
    let Appraisal {
        vault_id,
        total_value,
        remaining_types,
        appraised_positions,
        position_total,
        types_snapshot,
        mutation_seq_snapshot,
        external_pending,
    } = a;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
    assert!(remaining_types.is_empty(), errors::appraisal_incomplete());
    assert!(!external_pending, errors::appraisal_incomplete());
    assert!(appraised_positions.length() == position_total, errors::appraisal_incomplete());
    // Nothing may have moved since begin (same-PTB sessions invalidate).
    assert!(position_total == vault.position_count, errors::appraisal_mismatch());
    assert!(types_snapshot == vault.asset_types, errors::appraisal_mismatch());
    assert!(mutation_seq_snapshot == vault.mutation_seq, errors::appraisal_mismatch());
    events::emit_vault_appraised(vault_id, total_value, position_total);
    total_value
}

/// Permissionless mark refresh: run the SAME validation as every other
/// consume and discard the NAV — the only effect is the
/// `PositionAppraised` / `VaultAppraised` events carrying fresh marks.
/// Safe for anyone to call: it takes the vault immutably, so it can
/// neither move value nor skew a snapshot; a stale or skewed appraisal
/// aborts exactly as it would at deposit/fulfillment.
public fun crank_appraisal(vault: &TradingVault, appraisal: Appraisal) {
    let _ = consume_appraisal(vault, appraisal);
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
        price::quote_asset(att) == vault.config.accounting_asset,
        errors::price_asset_mismatch(),
    );
    assert_attestation_fresh(cfg, att, clock);
}

// ═══════════════════════ external account ═══════════════════════

/// Register (or rotate) the vault's external account. Admin-gated: the
/// account address and its limits are a protocol-trust decision, like
/// allowlisting an adapter. The pinned `equity_oracle` witness must be on
/// the oracle allowlist. Rotation repoints address/oracle/limits; live
/// exposure and the release window carry over.
public fun set_external_account(
    _: &AdminCap,
    vault: &mut TradingVault,
    reg: &OracleRegistry,
    account: address,
    equity_oracle: TypeName,
    budget_bps: u64,
    daily_release_bps: u64,
) {
    assert!(
        budget_bps <= (BPS_DENOM as u64) && daily_release_bps <= (BPS_DENOM as u64),
        errors::config_invalid(),
    );
    assert!(registry::is_oracle_allowed(reg, &equity_oracle), errors::oracle_not_allowed());
    if (vault.external.is_some()) {
        let ext = vault.external.borrow_mut();
        ext.account = account;
        ext.equity_oracle = equity_oracle;
        ext.budget_bps = budget_bps;
        ext.daily_release_bps = daily_release_bps;
    } else {
        vault.external.fill(ExternalAccount {
            account,
            equity_oracle,
            budget_bps,
            daily_release_bps,
            exposure: 0,
            released_in_window: 0,
            window_start_ms: 0,
        });
    };
    events::emit_external_account_set(
        object::id(vault),
        account,
        equity_oracle,
        budget_bps,
        daily_release_bps,
    );
}

/// Curator self-serve registration of an external account, authorized by
/// an ed25519 attestation from the protocol registrar (the hedge-signer
/// service) that `account` is a 2-of-2 FROST parent it co-holds. This is
/// FIRST-SET-ONLY: a replayed attestation can never re-point an already
/// registered account, and limits are capped well below the admin path's.
/// Re-pointing and above-cap budgets stay `set_external_account`
/// (AdminCap) decisions.
public fun set_external_account_attested(
    cap: &CuratorCap,
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    reg: &OracleRegistry,
    account: address,
    equity_oracle: TypeName,
    budget_bps: u64,
    daily_release_bps: u64,
    attestation: vector<u8>,
) {
    assert_current_cap(vault, cap);
    assert!(vault.external.is_none(), errors::external_already_set());
    assert!(
        budget_bps <= ATTESTED_MAX_BUDGET_BPS
            && daily_release_bps <= ATTESTED_MAX_DAILY_RELEASE_BPS,
        errors::attested_limits_exceeded(),
    );
    assert!(registry::is_oracle_allowed(reg, &equity_oracle), errors::oracle_not_allowed());

    let pubkey = registry::registrar_pubkey(cfg);
    assert!(!pubkey.is_empty(), errors::attestation_disabled());
    let msg = external_registration_message(object::id(vault).to_address(), account);
    assert!(sui::ed25519::ed25519_verify(&attestation, pubkey, &msg), errors::bad_attestation());

    vault.external.fill(ExternalAccount {
        account,
        equity_oracle,
        budget_bps,
        daily_release_bps,
        exposure: 0,
        released_in_window: 0,
        window_start_ms: 0,
    });
    events::emit_external_account_set(
        object::id(vault),
        account,
        equity_oracle,
        budget_bps,
        daily_release_bps,
    );
}

/// The exact bytes the registrar signs: domain tag ‖ vault id ‖ account,
/// each address as its raw 32 bytes. Public so the signer service and
/// tests build byte-identical messages.
public fun external_registration_message(vault_id: address, account: address): vector<u8> {
    let mut msg = EXTERNAL_REG_DOMAIN;
    msg.append(vault_id.to_bytes());
    msg.append(account.to_bytes());
    msg
}

/// Deregister the external account. Only once every released unit has
/// been returned — an appraisal must never silently drop live exposure.
public fun clear_external_account(_: &AdminCap, vault: &mut TradingVault) {
    assert!(vault.external.is_some(), errors::external_not_configured());
    let ExternalAccount { exposure, .. } = vault.external.extract();
    assert!(exposure == 0, errors::external_exposure_open());
    events::emit_external_account_cleared(object::id(vault));
}

/// Curator-gated, budgeted release of deposit-asset capital to the
/// registered external account — the ONLY vault outflow that does not
/// return in-transaction. Consumes a complete `Appraisal` so both limits
/// bind against true NAV at release time.
public fun release_external<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    appraisal: Appraisal,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    assert!(amount > 0, core_errors::zero_amount());
    assert!(vault.external.is_some(), errors::external_not_configured());
    let nav = consume_appraisal(vault, appraisal);

    let now = clock.timestamp_ms();
    let (account, exposure_after) = {
        let ext = vault.external.borrow_mut();
        if (now >= ext.window_start_ms + RELEASE_WINDOW_MS) {
            ext.window_start_ms = now;
            ext.released_in_window = 0;
        };
        let budget = nav * (ext.budget_bps as u128) / BPS_DENOM;
        assert!(
            (ext.exposure as u128) + (amount as u128) <= budget,
            errors::external_budget_exceeded(),
        );
        let daily = nav * (ext.daily_release_bps as u128) / BPS_DENOM;
        assert!(
            (ext.released_in_window as u128) + (amount as u128) <= daily,
            errors::external_rate_limited(),
        );
        ext.exposure = ext.exposure + amount;
        ext.released_in_window = ext.released_in_window + amount;
        (ext.account, ext.exposure)
    };
    transfer::public_transfer(
        coin::from_balance(take_balance_internal<T>(vault, amount), ctx),
        account,
    );
    events::emit_external_released(object::id(vault), account, amount, exposure_after, nav);
}

/// Repatriation: the registered account (and only it — the sweep tx is
/// sent BY the jointly-controlled address) pays deposit-asset funds back
/// into free balances, reducing exposure. Amounts beyond the recorded
/// exposure (venue profit) floor it at zero.
public fun return_external<T>(vault: &mut TradingVault, funds: Coin<T>, ctx: &TxContext) {
    assert!(vault.external.is_some(), errors::external_not_configured());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let amount = funds.value();
    assert!(amount > 0, core_errors::zero_amount());
    let exposure_after = {
        let ext = vault.external.borrow_mut();
        assert!(ctx.sender() == ext.account, errors::not_authorized());
        ext.exposure = if (ext.exposure > amount) { ext.exposure - amount } else { 0 };
        ext.exposure
    };
    put_balance_internal<T>(vault, funds.into_balance());
    events::emit_external_returned(object::id(vault), ctx.sender(), amount, exposure_after);
}

/// The external account's equity leg of an appraisal, in deposit-asset
/// units. Only the PINNED oracle-adapter witness may record it, and only
/// while that witness stays allowlisted (delisting is an instant kill
/// switch). The adapter owns how `equity` is derived — attested by a
/// keeper under guardrails, or computed on-chain from venue state.
///
/// Recordable exactly once, and only into an appraisal that actually
/// wants the leg: with zero live exposure `begin_appraisal` already
/// values the account at its by-construction zero, so a second (or
/// unwanted) leg aborts `already_appraised` rather than adding an
/// attested number on top. Composers gate on `external_exposure`.
public fun record_external_equity<W: drop>(
    vault: &TradingVault,
    reg: &OracleRegistry,
    a: &mut Appraisal,
    _witness: W,
    equity: u64,
) {
    assert!(a.vault_id == object::id(vault), errors::wrong_vault());
    assert!(vault.external.is_some(), errors::external_not_configured());
    let w = type_name::with_defining_ids<W>();
    assert!(w == vault.external.borrow().equity_oracle, errors::wrong_external_oracle());
    assert!(registry::is_oracle_allowed(reg, &w), errors::oracle_not_allowed());
    assert!(a.external_pending, errors::already_appraised());
    a.external_pending = false;
    a.total_value = a.total_value + (equity as u128);
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
    // Live external exposure is off-vault capital: it must be repatriated
    // (or written off via admin re-registration) before the terminal state.
    if (vault.external.is_some()) {
        assert!(vault.external.borrow().exposure == 0, errors::external_exposure_open());
    };
    let n = vault.asset_types.length();
    let clean = n == 0
        || (n == 1 && vault.asset_types.contains(&vault.config.accounting_asset));
    assert!(clean, errors::residual_assets());
    vault.state = VaultState::Closed;
    events::emit_vault_closed(object::id(vault));
}

public fun rotate_curator_by_curator(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    recipient: address,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
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

/// Curator opt-in/out for the `vault_mm` quote-collateral path.
public fun set_mm_release_enabled(vault: &mut TradingVault, cap: &CuratorCap, enabled: bool) {
    assert_current_cap(vault, cap);
    vault.config.mm_release_enabled = enabled;
    events::emit_mm_release_toggled(object::id(vault), enabled);
}

// ════════════════════════════ internals ════════════════════════════

fun assert_current_cap(vault: &TradingVault, cap: &CuratorCap) {
    assert!(cap.vault_id == object::id(vault), errors::wrong_vault());
    assert!(object::id(cap) == vault.curator_cap_id, errors::not_curator());
}

fun put_balance_internal<T>(vault: &mut TradingVault, funds: Balance<T>) {
    let key = BalanceKey<T> {};
    let t = type_name::with_defining_ids<T>();
    vault.mutation_seq = vault.mutation_seq + 1;
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
    vault.mutation_seq = vault.mutation_seq + 1;
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

public fun accounting_asset(vault: &TradingVault): TypeName { vault.config.accounting_asset }

/// Copy of the deposit/payout allowlist (always contains the accounting
/// asset).
public fun deposit_assets(vault: &TradingVault): VecSet<TypeName> { vault.config.deposit_assets }

public fun is_deposit_asset(vault: &TradingVault, t: &TypeName): bool {
    vault.config.deposit_assets.contains(t)
}

public fun is_quote_adapter(vault: &TradingVault, w: &TypeName): bool {
    vault.config.quote_adapters.contains(w)
}

/// (entry_haircut_bps, exit_haircut_bps).
public fun haircuts(vault: &TradingVault): (u64, u64) {
    (vault.config.entry_haircut_bps, vault.config.exit_haircut_bps)
}

/// Fields of the pending request at `seq`: (recipient, shares, basis,
/// payout_asset, requested_at_ms).
public fun queue_request(vault: &TradingVault, seq: u64): (address, u128, u64, TypeName, u64) {
    assert!(vault.queue.contains(seq), errors::request_missing());
    let r = vault.queue.borrow(seq);
    (r.recipient, r.shares, r.basis, r.payout_asset, r.requested_at_ms)
}

public fun queue_head(vault: &TradingVault): u64 { vault.queue_head }

public fun share_offset(): u128 { SHARE_OFFSET }

public fun total_shares(vault: &TradingVault): u128 { vault.total_shares }

public fun position_count(vault: &TradingVault): u64 { vault.position_count }

public fun free_balance_of<T>(vault: &TradingVault): u64 { free_balance_value<T>(vault) }

public fun curator_cap_id(vault: &TradingVault): ID { vault.curator_cap_id }

public fun creator(vault: &TradingVault): address { vault.creator }

public fun lockup_ms(vault: &TradingVault): u64 { vault.config.lockup_ms }

public fun curator_fee_bps(vault: &TradingVault): u64 { vault.config.curator_fee_bps }

public fun unwind_grace_ms(vault: &TradingVault): u64 { vault.config.unwind_grace_ms }

public fun mm_release_enabled(vault: &TradingVault): bool { vault.config.mm_release_enabled }

public fun pending_withdrawals(vault: &TradingVault): u64 {
    vault.queue_tail - vault.queue_head
}

public fun has_external_account(vault: &TradingVault): bool { vault.external.is_some() }

public fun external_account(vault: &TradingVault): address {
    assert!(vault.external.is_some(), errors::external_not_configured());
    vault.external.borrow().account
}

public fun external_exposure(vault: &TradingVault): u64 {
    if (vault.external.is_none()) { return 0 };
    vault.external.borrow().exposure
}

public fun external_equity_oracle(vault: &TradingVault): TypeName {
    assert!(vault.external.is_some(), errors::external_not_configured());
    vault.external.borrow().equity_oracle
}

/// (budget_bps, daily_release_bps, released_in_window, window_start_ms).
public fun external_limits(vault: &TradingVault): (u64, u64, u64, u64) {
    assert!(vault.external.is_some(), errors::external_not_configured());
    let ext = vault.external.borrow();
    (ext.budget_bps, ext.daily_release_bps, ext.released_in_window, ext.window_start_ms)
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
