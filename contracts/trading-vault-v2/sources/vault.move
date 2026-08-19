/// Curated trading vault v2 (docs/trading-vault-overhaul-plan.md):
/// tokenized, freely transferable `VaultPosition` claims; an immutable
/// Untranched / SeniorJunior capital structure with the §3.4a waterfall;
/// per-tranche FIFO withdrawal lanes under one global sequence (§3.6);
/// the §8.4b capital risk-state gates; the curator first-loss commitment
/// held in vault escrow (§2.2/§8.6); the generational junior reset
/// (§8.5); and the senior-first terminal settlement pool (§8.7).
///
/// Custody principles carried over from v1 unchanged:
/// 1. **Curator trades, never withdraws** — balances leave only into
///    allowlisted adapter code paths inside a `Session` hot potato.
/// 2. **Oracle-free trading, oracle-priced accounting** — NAV comes from
///    `PriceAttestation`s via the complete-and-same-transaction
///    `Appraisal` hot potato.
/// 3. Per-claim cost basis with exit-crystallized performance fees; fee
///    shares minted at the batch ratio into the SAME tranche (§3.5),
///    credited to the escrowed curator commitment position.
///
/// Share math keeps the v1 virtual offset per tranche:
///   shares = value × (S_t + O) / (nav_t + 1)
///   value  = shares × (nav_t + 1) / (S_t + O)
/// with u256 intermediates and floor division.
module vault_v2::vault;

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
use whitelist::whitelist::{Self, Whitelist};
use options_core::errors as core_errors;
use options_core::treasury::{Self, Treasury};

use vault_v2::capital::{Self, CapitalStructure, Tranche, TrancheBook};
use vault_v2::errors;
use vault_v2::events;
use vault_v2::fees;
use vault_v2::price::{Self, PriceAttestation};
use vault_v2::registry::{Self, IntegrationRegistry, OracleRegistry, VaultProtocolConfig};
use vault_v2::vault_position::{Self, VaultPosition};

const BPS_DENOM: u128 = 10_000;

/// Virtual shares standing against 1 virtual accounting-asset unit in
/// every share↔value conversion, per tranche (OZ decimals-offset
/// pattern; separate offsets per §3.4 even though the constants match).
const SHARE_OFFSET: u128 = 1_000_000;

/// Hard cap on the curator-set entry/exit haircuts.
const MAX_HAIRCUT_BPS: u64 = 500;

const LANE_SENIOR: u8 = 0;
const LANE_JUNIOR: u8 = 1;

public enum VaultState has copy, drop, store {
    Open,
    /// Unwind only: no deposits, sessions still run so the curator (or,
    /// permissionlessly, force cranks) can flatten positions.
    Closing,
    /// Terminal: the one-time settlement snapshot (§8.7) freezes
    /// per-share entitlements; positions redeem against the pool
    /// forever. "Fully closed" means SETTLED, not zero outstanding
    /// shares.
    Closed,
}

public struct VaultConfig has copy, drop, store {
    /// Unit of account. Fixed at creation.
    accounting_asset: TypeName,
    /// Assets users may deposit and request payouts in.
    deposit_assets: VecSet<TypeName>,
    lockup_ms: u64,
    curator_fee_bps: u64,
    /// Queue-head age after which permissionless force-unwind sessions
    /// unlock and the accounting-asset grace fallback opens.
    unwind_grace_ms: u64,
    entry_haircut_bps: u64,
    exit_haircut_bps: u64,
    /// Adapter witnesses the curator has opted into for QUOTE sessions.
    quote_adapters: VecSet<TypeName>,
    deposits_paused: bool,
    /// Opt-in for the `vault_mm` release path.
    mm_release_enabled: bool,
}

/// A queued exit. The position object was CONSUMED at request time
/// (§2.3, selected design 1); these are its escrowed accounting fields.
/// Queued shares stay outstanding — they keep participating in P&L (and,
/// for senior, hurdle accrual) until fulfilled.
public struct WithdrawRequest has store {
    position_id: ID,
    recipient: address,
    tranche: Tranche,
    capital_generation: u64,
    shares: u128,
    basis: u64,
    payout_asset: TypeName,
    requested_at_ms: u64,
    /// Index of this request inside its lane's entry table.
    lane_idx: u64,
}

/// One FIFO withdrawal lane (§3.6): `entries[head..tail)` maps
/// lane-local indices to global sequence numbers. Strict FIFO within a
/// lane; cross-lane order is lowest-global-sequence-first among payable
/// heads.
public struct Lane has store {
    entries: Table<u64, u64>,
    head: u64,
    tail: u64,
}

/// Frozen terminal entitlements (§8.7): senior first, pro rata within a
/// tranche, wiped generations at zero. Redemption is a pure table
/// lookup + balance split forever after.
public struct SettlementPool has store {
    senior_pool: u64,
    senior_supply: u128,
    junior_pool: u64,
    junior_supply: u128,
    active_junior_generation: u64,
    /// Curator performance fees crystallized at settlement redemptions,
    /// claimable by the current cap (share mints are impossible after
    /// the snapshot).
    curator_fees_accrued: u64,
}

public struct TradingVault has key {
    id: UID,
    creator: address,
    curator_cap_id: ID,
    state: VaultState,
    config: VaultConfig,
    /// Immutable at creation (§3.2).
    capital: CapitalStructure,
    book: TrancheBook,
    /// §9.2: the exact spec version + content hash governing issuance.
    terms_version: u64,
    spec_hash: vector<u8>,
    /// Every asset type with a non-zero free balance.
    asset_types: VecSet<TypeName>,
    position_count: u64,
    /// Bumped by every free-balance mutation; appraisals snapshot it.
    mutation_seq: u64,
    /// Bumped by every capital mutation (supplies, claim, accrual,
    /// generation); appraisals snapshot it too (§3.7).
    capital_seq: u64,
    /// Requests keyed by GLOBAL sequence; lanes order them.
    requests: Table<u64, WithdrawRequest>,
    senior_lane: Lane,
    junior_lane: Lane,
    next_global_seq: u64,
    /// §8.6: marked commitment below the floor ⇒ the §8.4b gate set
    /// (minus junior-lane effects). Recomputed at every capital sync;
    /// set pessimistically on rotation/reset until the next sync.
    curator_commitment_breached: bool,
    external: Option<ExternalAccount>,
    settlement: Option<SettlementPool>,
}

/// Capital deployed to a venue the vault cannot custody at the Move
/// level — unchanged from v1 (budget/rate-limit enforced releases,
/// pinned equity oracle, returns only from the registered address).
public struct ExternalAccount has store {
    account: address,
    equity_oracle: TypeName,
    budget_bps: u64,
    daily_release_bps: u64,
    exposure: u64,
    released_in_window: u64,
    window_start_ms: u64,
}

/// Transferable curator role, unchanged from v1.
public struct CuratorCap has key, store {
    id: UID,
    vault_id: ID,
}

public struct BalanceKey<phantom T> has copy, drop, store {}

public struct PositionKey has copy, drop, store { id: ID }

/// Which adapter custodied a position; only that adapter may take or
/// appraise it.
public struct PositionTagKey has copy, drop, store { id: ID }

/// Escrow slot for the curator's commitment position (§2.2): keyed by
/// cap id + tranche wire code. The position inside is a normal
/// `VaultPosition` by type — it is simply not in anyone's wallet, which
/// is the entire enforcement mechanism. It is NOT an appraisal leg:
/// it holds shares, not assets.
public struct CommitmentKey has copy, drop, store { cap_id: ID, tranche: u8 }

/// Curator-operation hot potato: everything a session takes must resolve
/// back into the vault this same transaction. No abilities.
public struct Session {
    vault_id: ID,
    adapter: TypeName,
    forced: bool,
    taken: VecMap<TypeName, u64>,
    returned: VecMap<TypeName, u64>,
    positions_added: u64,
    positions_removed: u64,
}

/// NAV hot potato: consumed only when every held asset type and every
/// custodied position has been valued, and neither balances nor capital
/// state moved since `begin`.
public struct Appraisal {
    vault_id: ID,
    total_value: u128,
    remaining_types: VecSet<TypeName>,
    appraised_positions: VecSet<ID>,
    position_total: u64,
    types_snapshot: VecSet<TypeName>,
    mutation_seq_snapshot: u64,
    capital_seq_snapshot: u64,
    external_pending: bool,
}

/// Fulfillment hot potato: one appraisal's per-tranche waterfall ratios
/// plus the batch's asset prices, locked for the whole batch (§3.4).
/// An untranched vault's single book rides in the junior fields.
public struct Fulfillment {
    vault_id: ID,
    senior_nav: u128,
    senior_supply: u128,
    junior_nav: u128,
    junior_supply: u128,
    /// Accrued claim + supply at batch lock, for the pro-rata claim
    /// reduction (§3.3).
    locked_claim: u128,
    junior_blocked: bool,
    prices: VecMap<TypeName, u128>,
}

/// Rolling window for the external-account release rate limit.
const RELEASE_WINDOW_MS: u64 = 86_400_000;

// Curator self-serve (attested) registration limits.
const ATTESTED_MAX_BUDGET_BPS: u64 = 2_000;
const ATTESTED_MAX_DAILY_RELEASE_BPS: u64 = 1_000;

/// Domain separator for registrar attestations (18 bytes). Kept
/// byte-identical to v1 so completed FROST ceremonies survive.
const EXTERNAL_REG_DOMAIN: vector<u8> = b"tv_external_reg_v1";

// ═══════════════════════════════ creation ═══════════════════════════════

/// Permissionless behind the ingress whitelist. The creator IS the
/// initial curator. The capital structure is immutable: `structure_code`
/// 0 = Untranched (all tranche parameters must be zero), 1 = SeniorJunior
/// (validated against the protocol floors/caps). `upside_code`
/// 0 = PreferredOnly, 1 = CappedParticipating, 2 = UncappedParticipating.
#[allow(lint(self_transfer))]
public fun create_vault<T>(
    cfg: &VaultProtocolConfig,
    wl: &Whitelist,
    lockup_ms: u64,
    curator_fee_bps: u64,
    unwind_grace_ms: u64,
    structure_code: u8,
    senior_hurdle_bps_annual: u64,
    target_junior_bps: u64,
    maintenance_junior_bps: u64,
    upside_code: u8,
    residual_participation_bps: u64,
    total_return_cap_bps: u64,
    terms_version: u64,
    spec_hash: vector<u8>,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(curator_fee_bps <= registry::max_curator_fee_bps(cfg), errors::fee_too_high());

    let capital_structure = if (structure_code == 0) {
        assert!(
            senior_hurdle_bps_annual == 0
                && target_junior_bps == 0
                && maintenance_junior_bps == 0
                && upside_code == 0
                && residual_participation_bps == 0
                && total_return_cap_bps == 0,
            errors::config_invalid(),
        );
        capital::untranched_structure()
    } else if (structure_code == 1) {
        capital::senior_junior_structure(
            cfg,
            senior_hurdle_bps_annual,
            target_junior_bps,
            maintenance_junior_bps,
            upside_code,
            residual_participation_bps,
            total_return_cap_bps,
        )
    } else {
        abort errors::config_invalid()
    };

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
        capital: capital_structure,
        book: capital::new_book(clock.timestamp_ms()),
        terms_version,
        spec_hash,
        asset_types: vec_set::empty(),
        position_count: 0,
        mutation_seq: 0,
        capital_seq: 0,
        requests: table::new(ctx),
        senior_lane: Lane { entries: table::new(ctx), head: 0, tail: 0 },
        junior_lane: Lane { entries: table::new(ctx), head: 0, tail: 0 },
        next_global_seq: 0,
        curator_commitment_breached: false,
        external: option::none(),
        settlement: option::none(),
    };
    let vault_id = object::id(&vault);
    let cap = CuratorCap { id: object::new(ctx), vault_id };
    let cap_id = object::id(&cap);
    vault.curator_cap_id = cap_id;

    let (up_code, part_bps, cap_bps) = capital::upside_fields(&vault.capital);
    events::emit_vault_created(
        vault_id,
        vault.creator,
        cap_id,
        vault.config.accounting_asset,
        lockup_ms,
        curator_fee_bps,
        unwind_grace_ms,
        structure_code,
        capital::senior_hurdle_bps_annual(&vault.capital),
        capital::target_junior_bps(&vault.capital),
        capital::maintenance_junior_bps(&vault.capital),
        up_code,
        part_bps,
        cap_bps,
        terms_version,
        vault.spec_hash,
    );
    transfer::public_transfer(cap, ctx.sender());
    transfer::share_object(vault);
    vault_id
}

// ═══════════════════════════ capital sync ═══════════════════════════

/// Accrue the hurdle, run the waterfall, re-derive the risk state and
/// the curator-commitment flag, and emit `CapitalSynced`. Runs inside
/// EVERY mutable consumed-appraisal path — the single choke point that
/// keeps the §8.4b gates and lane blocks honest. Returns
/// `(senior_nav, junior_nav)` at `nav`.
fun sync_capital(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    nav: u128,
    now_ms: u64,
): (u128, u128) {
    let had_proposal = capital::has_reset_proposal(&vault.book);
    let old_generation = capital::active_junior_generation(&vault.book);
    capital::accrue(&mut vault.book, &vault.capital, now_ms);
    let (senior_nav, junior_nav) = capital::waterfall(
        &vault.capital,
        nav,
        capital::senior_claim(&vault.book),
        capital::senior_principal_basis(&vault.book),
    );
    let (old_state, new_state) =
        capital::update_risk_state(&mut vault.book, &vault.capital, nav, junior_nav, now_ms);
    if (old_state != new_state) {
        events::emit_risk_state_changed(object::id(vault), old_state, new_state, now_ms);
    };
    if (had_proposal && !capital::has_reset_proposal(&vault.book)) {
        events::emit_junior_reset_cancelled(object::id(vault), old_generation);
    };

    vault.curator_commitment_breached = commitment_breached(vault, cfg, nav, junior_nav);
    vault.capital_seq = vault.capital_seq + 1;

    events::emit_capital_synced(
        object::id(vault),
        nav,
        senior_nav,
        junior_nav,
        capital::senior_claim(&vault.book),
        capital::senior_shares(&vault.book),
        capital::junior_shares(&vault.book),
        capital::risk_state_code(&capital::risk_state(&vault.book)),
        capital::active_junior_generation(&vault.book),
        vault.curator_commitment_breached,
    );
    (senior_nav, junior_nav)
}

/// §8.6: is the current cap's escrowed commitment position, marked at
/// the commitment tranche's fresh ratio, below
/// `min_curator_commitment_bps` of total NAV? Only binds while Open and
/// while the protocol enforcement switch is on.
fun commitment_breached(
    vault: &TradingVault,
    cfg: &VaultProtocolConfig,
    nav: u128,
    junior_nav: u128,
): bool {
    if (vault.state != VaultState::Open) { return false };
    if (!registry::enforce_curator_share(cfg)) { return false };
    let floor_bps = registry::min_curator_commitment_bps(cfg);
    if (floor_bps == 0 || nav == 0) { return false };
    let key = CommitmentKey {
        cap_id: vault.curator_cap_id,
        tranche: capital::tranche_code(&commitment_tranche(vault)),
    };
    let marked: u128 = if (dof::exists(&vault.id, key)) {
        let p: &VaultPosition = dof::borrow(&vault.id, key);
        let wiped = capital::is_junior(&vault_position::tranche(p))
            && vault_position::capital_generation(p)
                < capital::active_junior_generation(&vault.book);
        if (wiped) { 0 } else {
            let t = commitment_tranche(vault);
            let t_nav = if (capital::is_tranched(&vault.capital)) { junior_nav } else { nav };
            let supply = capital::supply_of(&vault.book, &t);
            fees::claim_value(vault_position::shares(p), t_nav, supply, SHARE_OFFSET) as u128
        }
    } else { 0 };
    marked * BPS_DENOM < (floor_bps as u128) * nav
}

fun commitment_tranche(vault: &TradingVault): Tranche {
    if (capital::is_tranched(&vault.capital)) {
        capital::tranche_from_code(2) // Junior — first-loss capital (§8.6)
    } else {
        capital::tranche_from_code(0)
    }
}

/// §8.4b master switch: a tranched vault in any non-healthy capital
/// state, or any vault whose curator commitment is breached, is
/// risk-off — deployment outflows stop, unwinding continues.
public fun is_risk_off(vault: &TradingVault): bool {
    !capital::is_healthy(&vault.book) || vault.curator_commitment_breached
}

// ══════════════════════════ deposits ══════════════════════════

/// Deposit into `tranche_code` (0 untranched / 1 senior / 2 junior),
/// minting and RETURNING a freely transferable `VaultPosition` — the
/// caller's PTB decides where it goes (wallet, kiosk, wrapper). Each
/// deposit mints a NEW position with its own lockup; earlier positions
/// keep their original expiry (§2.3, intended change from v1).
///
/// Gates: ingress whitelist + protocol pause + vault Open + tranche
/// valid for the structure; senior additionally requires `Healthy` state
/// and the post-deposit junior buffer to meet `target_junior_bps`
/// (§8.4); all ordinary deposits are blocked in `Impaired` /
/// `ResetPending` (§8.5) and senior also in `CoverageBreach`.
public fun deposit<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    wl: &Whitelist,
    appraisal: Appraisal,
    funds: Coin<T>,
    att: Option<PriceAttestation>,
    tranche_code: u8,
    clock: &Clock,
    ctx: &mut TxContext,
): VaultPosition {
    let (position, _) = deposit_internal<T>(
        vault,
        cfg,
        wl,
        appraisal,
        funds,
        att,
        tranche_code,
        false,
        clock,
        ctx,
    );
    position
}

/// Curator commitment funding (§2.2/§8.6): identical valuation and
/// share math, but the minted claim lands in (or merges into) the
/// escrowed commitment position for the current cap instead of a
/// wallet. The commitment tranche is junior for a tranched vault,
/// untranched otherwise.
public fun deposit_into_commitment<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    wl: &Whitelist,
    cap: &CuratorCap,
    appraisal: Appraisal,
    funds: Coin<T>,
    att: Option<PriceAttestation>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    let tranche_code = capital::tranche_code(&commitment_tranche(vault));
    let (position, nav_after) = deposit_internal<T>(
        vault,
        cfg,
        wl,
        appraisal,
        funds,
        att,
        tranche_code,
        true,
        clock,
        ctx,
    );
    let key = CommitmentKey { cap_id: vault.curator_cap_id, tranche: tranche_code };
    if (dof::exists(&vault.id, key)) {
        let existing: &VaultPosition = dof::borrow(&vault.id, key);
        let wiped = capital::is_junior(&vault_position::tranche(existing))
            && vault_position::capital_generation(existing)
                < capital::active_junior_generation(&vault.book);
        if (wiped) {
            // A pre-reset commitment is a permanently zero claim (§8.5):
            // burn it and start the new-generation commitment fresh.
            let old: VaultPosition = dof::remove(&mut vault.id, key);
            let (old_id, _, old_shares, _, _, old_gen) = vault_position::consume(old);
            events::emit_wiped_position_burned(object::id(vault), old_id, old_gen, old_shares);
            dof::add(&mut vault.id, key, position);
        } else {
            let existing: &mut VaultPosition = dof::borrow_mut(&mut vault.id, key);
            vault_position::merge(existing, position);
        }
    } else {
        dof::add(&mut vault.id, key, position);
    };
    // Funding the commitment is exactly what cures a commitment breach:
    // re-test immediately so risk-on can resume without a second crank.
    let (_, junior_nav) = capital::waterfall(
        &vault.capital,
        nav_after,
        capital::senior_claim(&vault.book),
        capital::senior_principal_basis(&vault.book),
    );
    vault.curator_commitment_breached = commitment_breached(vault, cfg, nav_after, junior_nav);
}

/// Returns the minted position and the post-deposit total NAV.
fun deposit_internal<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    wl: &Whitelist,
    appraisal: Appraisal,
    funds: Coin<T>,
    att: Option<PriceAttestation>,
    tranche_code: u8,
    for_commitment: bool,
    clock: &Clock,
    ctx: &mut TxContext,
): (VaultPosition, u128) {
    // Ingress gate (whitelist + pause). Exits never check this.
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(!registry::is_paused(cfg), errors::protocol_paused());
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(!vault.config.deposits_paused, errors::deposits_paused());
    let t = type_name::with_defining_ids<T>();
    assert!(vault.config.deposit_assets.contains(&t), errors::asset_not_allowed());
    let amount = funds.value();
    assert!(amount > 0, core_errors::zero_amount());

    let tranche = capital::tranche_from_code(tranche_code);
    if (capital::is_tranched(&vault.capital)) {
        assert!(!capital::is_untranched_tranche(&tranche), errors::wrong_tranche());
    } else {
        assert!(capital::is_untranched_tranche(&tranche), errors::wrong_tranche());
    };

    // Value the deposit in accounting-asset units (entry haircut damps
    // oracle error on non-accounting flows).
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

    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    let (_senior_nav, junior_nav) = sync_capital(vault, cfg, nav, now);

    // Capital-state deposit gates (§8.4/§8.5).
    let state = capital::risk_state_code(&capital::risk_state(&vault.book));
    if (state == 2 || state == 3) {
        // Impaired / ResetPending: recapitalization only via
        // `execute_junior_reset`.
        abort errors::deposits_blocked_by_state()
    };
    if (capital::is_senior(&tranche)) {
        assert!(state == 0, errors::deposits_blocked_by_state());
        // Post-deposit target buffer (§8.4 policy 3, higher threshold):
        // junior NAV is unchanged by a senior deposit; total grows by it.
        let target = capital::target_junior_bps(&vault.capital) as u128;
        assert!(
            (junior_nav as u256) * (BPS_DENOM as u256)
                >= (target as u256) * ((nav + (value as u128)) as u256),
            errors::senior_buffer_breached(),
        );
    };

    // Price against the tranche book (virtual offset per tranche, §3.4).
    let tranche_nav = if (capital::is_senior(&tranche)) { _senior_nav } else if (
        capital::is_junior(&tranche)
    ) { junior_nav } else { nav };
    let supply = capital::supply_of(&vault.book, &tranche);
    // A wiped tranche (shares outstanding, nothing left) cannot price
    // deposits; junior recapitalization goes through the reset.
    assert!(supply == 0 || tranche_nav > 0, errors::vault_dead());
    let shares = fees::shares_for_value(value, tranche_nav, supply, SHARE_OFFSET);
    assert!(shares > 0, core_errors::zero_amount());

    put_balance_internal<T>(vault, funds.into_balance());
    capital::on_deposit(&mut vault.book, &tranche, value, shares);
    vault.capital_seq = vault.capital_seq + 1;

    let generation = if (capital::is_junior(&tranche)) {
        capital::active_junior_generation(&vault.book)
    } else { 0 };
    let locked_until_ms = now + vault.config.lockup_ms;
    let position = vault_position::mint(
        object::id(vault),
        tranche,
        shares,
        value,
        locked_until_ms,
        generation,
        ctx,
    );

    // Re-test the curator commitment at the POST-deposit NAV: the sync
    // above ran at the pre-deposit value (zero at genesis), and a
    // deposit that grows the vault can push the marked commitment below
    // the floor.
    let post_nav = nav + (value as u128);
    let post_junior = if (capital::is_senior(&tranche)) { junior_nav } else {
        junior_nav + (value as u128)
    };
    vault.curator_commitment_breached = commitment_breached(vault, cfg, post_nav, post_junior);

    events::emit_deposited(
        object::id(vault),
        ctx.sender(),
        if (for_commitment) { option::some(object::id(&position)) } else { option::none() },
        object::id(&position),
        tranche_code,
        generation,
        t,
        amount,
        value,
        shares,
        capital::supply_of(&vault.book, &tranche),
        locked_until_ms,
    );
    (position, nav + (value as u128))
}

// ═══════════════════════ deposit-asset allowlist ═══════════════════════

/// Curator-gated: allow `T` for deposits and payout requests.
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

/// Curator-gated: stop accepting `T`. Never the accounting asset;
/// pending payout requests in `T` still settle in `T`.
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

/// Queue a withdrawal by CONSUMING a whole position object (§2.3 —
/// partial exits are "split, then request"), payable in `P`. Never
/// whitelist-gated. Crystallization happens at fulfillment, so queued
/// shares keep earning the vault's P&L until paid. Blocked once the
/// vault is Closed — wallet-held positions then redeem against the
/// settlement pool instead (§8.7).
public fun request_withdraw<P>(
    vault: &mut TradingVault,
    position: VaultPosition,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(vault.state != VaultState::Closed, errors::queue_settled());
    assert!(vault_position::vault_id(&position) == object::id(vault), errors::wrong_position_vault());
    let payout = assert_payout_asset<P>(vault);
    let now = clock.timestamp_ms();
    assert!(now >= vault_position::locked_until_ms(&position), errors::still_locked());
    // Wiped junior claims have no exit value; burn them instead of
    // wedging the lane with zero-value requests.
    let (position_id, tranche, shares, basis, _, generation) = vault_position::consume(position);
    if (capital::is_junior(&tranche)) {
        assert!(
            generation == capital::active_junior_generation(&vault.book),
            errors::position_wiped(),
        );
    };

    let global_seq = vault.next_global_seq;
    vault.next_global_seq = global_seq + 1;
    let lane_code = lane_code_of(&tranche);
    let lane = if (lane_code == LANE_JUNIOR) { &mut vault.junior_lane } else {
        &mut vault.senior_lane
    };
    let lane_idx = lane.tail;
    lane.entries.add(lane_idx, global_seq);
    lane.tail = lane_idx + 1;

    vault.requests.add(
        global_seq,
        WithdrawRequest {
            position_id,
            recipient: ctx.sender(),
            tranche,
            capital_generation: generation,
            shares,
            basis,
            payout_asset: payout,
            requested_at_ms: now,
            lane_idx,
        },
    );
    events::emit_withdraw_requested(
        object::id(vault),
        global_seq,
        lane_code,
        position_id,
        ctx.sender(),
        capital::tranche_code(&tranche),
        generation,
        shares,
        basis,
        payout,
        now,
    );
}

/// Re-point a pending request's payout asset (recipient-only) — the
/// unwedge lever when the vault cannot source the requested asset.
public fun amend_payout_asset<P>(vault: &mut TradingVault, global_seq: u64, ctx: &TxContext) {
    assert!(vault.requests.contains(global_seq), errors::request_missing());
    let payout = assert_payout_asset<P>(vault);
    let req = vault.requests.borrow_mut(global_seq);
    assert!(req.recipient == ctx.sender(), errors::not_authorized());
    req.payout_asset = payout;
    events::emit_payout_asset_amended(object::id(vault), global_seq, payout);
}

fun assert_payout_asset<P>(vault: &TradingVault): TypeName {
    let p = type_name::with_defining_ids<P>();
    assert!(vault.config.deposit_assets.contains(&p), errors::asset_not_allowed());
    p
}

fun lane_code_of(tranche: &Tranche): u8 {
    if (capital::is_senior(tranche)) { LANE_SENIOR } else { LANE_JUNIOR }
}

/// Advance a lane's head past entries removed out of order (settlement
/// cleanups), returning the head's global sequence if the lane is
/// non-empty.
fun lane_head_seq(lane: &mut Lane): Option<u64> {
    while (lane.head < lane.tail && !lane.entries.contains(lane.head)) {
        lane.head = lane.head + 1;
    };
    if (lane.head == lane.tail) { option::none() } else {
        option::some(*lane.entries.borrow(lane.head))
    }
}

// ═══════════════════════════ fulfillment ═══════════════════════════

/// Open a fulfillment batch: consumes a complete appraisal, syncs
/// capital, locks BOTH tranche crystallization ratios and the accrued
/// claim (§3.4), and validates one attestation per payout asset the
/// batch will touch. Fulfillment never runs once Closed — the
/// settlement pool replaces lane cranking (§8.7).
public fun begin_fulfillment(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    atts: vector<PriceAttestation>,
    clock: &Clock,
): Fulfillment {
    assert!(vault.state != VaultState::Closed, errors::queue_settled());
    let nav = consume_appraisal(vault, appraisal);
    let (senior_nav, junior_nav) = sync_capital(vault, cfg, nav, clock.timestamp_ms());
    let senior_supply = capital::senior_shares(&vault.book);
    let junior_supply = capital::junior_shares(&vault.book);
    assert!(
        senior_supply + junior_supply > 0 || pending_withdrawals(vault) == 0,
        errors::vault_dead(),
    );

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

    let tranched = capital::is_tranched(&vault.capital);
    Fulfillment {
        vault_id: object::id(vault),
        senior_nav,
        senior_supply,
        junior_nav: if (tranched) { junior_nav } else { nav },
        junior_supply,
        locked_claim: capital::senior_claim(&vault.book),
        junior_blocked: tranched && capital::is_junior_blocked(&vault.book),
        prices,
    }
}

/// Permissionless crank: among the lane heads that are currently payable
/// in `P`, fulfill the one with the LOWEST global sequence (§3.6),
/// crystallizing at its tranche's batch-locked ratio. Within a lane,
/// order is strictly FIFO. A junior head is unpayable while the vault is
/// class-blocked; a class-blocked junior lane never stalls senior, and
/// vice versa. When nothing is blocked this reduces exactly to a single
/// global FIFO.
///
/// Per request (all floor division, offset-adjusted tranche ratio):
///   value        = shares × (nav_t + 1) / (S_t + OFFSET)
///   fee split    = fees::crystallize(value, basis, …)
///   payout       = value − gross_fee, converted to `P` at the batch
///                  price (exit haircut inflates the price)
/// The curator's net fee is minted as shares at the SAME tranche ratio
/// into the escrowed commitment position; a senior fee mint credits the
/// senior claim in the same batch (§3.5). Returns false — a no-op — when
/// no head is payable in `P` or the free `P` balance cannot fund the
/// chosen head.
public fun fulfill_next<P>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    f: &mut Fulfillment,
    clock: &Clock,
    ctx: &mut TxContext,
): bool {
    assert!(f.vault_id == object::id(vault), errors::wrong_vault());
    let p = type_name::with_defining_ids<P>();
    let is_accounting = p == vault.config.accounting_asset;
    let now = clock.timestamp_ms();

    // Peek both lane heads and pick the lowest payable global sequence.
    let senior_head = lane_head_seq(&mut vault.senior_lane);
    let junior_head = lane_head_seq(&mut vault.junior_lane);
    let mut chosen: Option<u64> = option::none();
    let mut i = 0u8;
    while (i < 2) {
        let head = if (i == 0) { &senior_head } else { &junior_head };
        if (head.is_some()) {
            let seq = *head.borrow();
            let req = vault.requests.borrow(seq);
            let class_blocked = i == 1 && f.junior_blocked
                && capital::is_junior(&req.tranche);
            let wiped = capital::is_junior(&req.tranche)
                && req.capital_generation < capital::active_junior_generation(&vault.book);
            let payable = if (class_blocked) { false } else if (wiped) {
                // Zero-value claims settle in any asset with no funding.
                true
            } else {
                req.payout_asset == p
                    || (is_accounting
                        && now > req.requested_at_ms + vault.config.unwind_grace_ms)
            };
            if (payable && (chosen.is_none() || seq < *chosen.borrow())) {
                chosen = option::some(seq);
            };
        };
        i = i + 1;
    };
    if (chosen.is_none()) { return false };
    let seq = chosen.destroy_some();

    let (value, payout_n, gross_fee, protocol_cut, wiped) = {
        let req = vault.requests.borrow(seq);
        let wiped = capital::is_junior(&req.tranche)
            && req.capital_generation < capital::active_junior_generation(&vault.book);
        if (wiped) {
            (0, 0, 0, 0, true)
        } else {
            let (t_nav, t_supply) = if (capital::is_senior(&req.tranche)) {
                (f.senior_nav, f.senior_supply)
            } else { (f.junior_nav, f.junior_supply) };
            let value = fees::claim_value(req.shares, t_nav, t_supply, SHARE_OFFSET);
            let (_, gross_fee, protocol_cut, _) = fees::crystallize(
                value,
                req.basis,
                vault.config.curator_fee_bps,
                registry::protocol_fee_bps(cfg),
            );
            (value, value - gross_fee, gross_fee, protocol_cut, false)
        }
    };

    // Convert accounting-unit obligations into `P` units at the batch
    // price (exit haircut ⇒ slightly fewer units out).
    let (payout_units, cut_units, price_used) = if (is_accounting || wiped) {
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

    // All-or-nothing per request.
    if ((payout_units as u128) + (cut_units as u128) > (free_balance_value<P>(vault) as u128)) {
        return false
    };

    let WithdrawRequest {
        position_id: _,
        recipient,
        tranche,
        capital_generation,
        shares,
        basis,
        payout_asset: _,
        requested_at_ms: _,
        lane_idx,
    } = vault.requests.remove(seq);
    let lane_code = lane_code_of(&tranche);
    let lane = if (lane_code == LANE_JUNIOR) { &mut vault.junior_lane } else {
        &mut vault.senior_lane
    };
    lane.entries.remove(lane_idx);

    let curator_net = gross_fee - protocol_cut;
    let profit = if (value > basis) { value - basis } else { 0 };

    if (!wiped) {
        capital::on_fulfill(&mut vault.book, &tranche, shares, f.locked_claim, f.senior_supply);
    };

    // Curator fee: mint shares into the escrowed commitment position at
    // the batch ratio (value stays in the vault — PPS neutral). Fee
    // shares carry no fresh lockup.
    let minted = if (curator_net > 0) {
        let (t_nav, t_supply) = if (capital::is_senior(&tranche)) {
            (f.senior_nav, f.senior_supply)
        } else { (f.junior_nav, f.junior_supply) };
        let m = fees::shares_for_value(curator_net, t_nav, t_supply, SHARE_OFFSET);
        if (m > 0) {
            capital::on_fee_mint(&mut vault.book, &tranche, m, curator_net);
            credit_commitment(vault, &tranche, m, curator_net, ctx);
        };
        m
    } else { 0 };
    vault.capital_seq = vault.capital_seq + 1;

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
        lane_code,
        recipient,
        capital::tranche_code(&tranche),
        capital_generation,
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
        capital::supply_of(&vault.book, &tranche),
    );
    true
}

/// Credit `shares`/`basis` of fee mint into the current cap's escrowed
/// commitment position for `tranche`, creating the slot on first use
/// (and replacing a wiped-generation escrow).
fun credit_commitment(
    vault: &mut TradingVault,
    tranche: &Tranche,
    shares: u128,
    basis: u64,
    ctx: &mut TxContext,
) {
    let generation = if (capital::is_junior(tranche)) {
        capital::active_junior_generation(&vault.book)
    } else { 0 };
    let key = CommitmentKey {
        cap_id: vault.curator_cap_id,
        tranche: capital::tranche_code(tranche),
    };
    if (dof::exists(&vault.id, key)) {
        let wiped = {
            let existing: &VaultPosition = dof::borrow(&vault.id, key);
            capital::is_junior(&vault_position::tranche(existing))
                && vault_position::capital_generation(existing) < generation
        };
        if (wiped) {
            let old: VaultPosition = dof::remove(&mut vault.id, key);
            let (old_id, _, old_shares, _, _, old_gen) = vault_position::consume(old);
            events::emit_wiped_position_burned(object::id(vault), old_id, old_gen, old_shares);
        } else {
            let existing: &mut VaultPosition = dof::borrow_mut(&mut vault.id, key);
            vault_position::credit(existing, shares, basis);
            return
        }
    };
    let position =
        vault_position::mint(object::id(vault), *tranche, shares, basis, 0, generation, ctx);
    dof::add(&mut vault.id, key, position);
}

public fun end_fulfillment(vault: &TradingVault, f: Fulfillment) {
    let Fulfillment {
        vault_id,
        senior_nav: _,
        senior_supply: _,
        junior_nav: _,
        junior_supply: _,
        locked_claim: _,
        junior_blocked: _,
        prices: _,
    } = f;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
}

/// Convenience batch crank for the all-accounting case: fulfill payable
/// heads (grace fallbacks included) until one can't be paid.
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
/// or Closing. In a risk-off capital state the session opens with
/// FORCED semantics — `take` aborts, `put`/position flows work — so the
/// curator can still flatten positions but cannot deploy (§8.4b).
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
    new_session(vault, adapter, is_risk_off(vault))
}

/// Permissionless conservative session: unlocked when the vault is
/// Closing, or when the OLDEST queue head across both lanes — payable or
/// class-blocked (§3.6: a blocked junior lane still counts as unmet exit
/// demand) — has aged past `unwind_grace_ms`. Cannot `take`.
/// Deliberately NOT allowlist-gated: delisting an adapter must never
/// strand the exit path for value already deployed under it.
#[allow(lint(unused_object_with_fields))]
public fun begin_force_session<W: drop>(
    vault: &mut TradingVault,
    _reg: &IntegrationRegistry,
    _witness: W,
    clock: &Clock,
): Session {
    let adapter = type_name::with_defining_ids<W>();
    let ready = vault.state == VaultState::Closing || {
        let oldest = oldest_head_requested_at(vault);
        oldest.is_some()
            && clock.timestamp_ms() > *oldest.borrow() + vault.config.unwind_grace_ms
    };
    assert!(ready, errors::unwind_not_ready());
    new_session(vault, adapter, true)
}

fun oldest_head_requested_at(vault: &mut TradingVault): Option<u64> {
    let senior_head = lane_head_seq(&mut vault.senior_lane);
    let junior_head = lane_head_seq(&mut vault.junior_lane);
    // The lowest global sequence across lanes is the oldest request.
    let seq = if (senior_head.is_some() && junior_head.is_some()) {
        let s = *senior_head.borrow();
        let j = *junior_head.borrow();
        option::some(s.min(j))
    } else if (senior_head.is_some()) { senior_head } else { junior_head };
    if (seq.is_none()) { return option::none() };
    option::some(vault.requests.borrow(seq.destroy_some()).requested_at_ms)
}

/// Permissionless, take-capable QUOTE session — the settlement path for
/// adapters that fill the vault's signed maker quotes against free
/// balances. Triple-gated (protocol allowlist + curator opt-in + Open),
/// and additionally ABORTS in every risk-off state: quote fills draw
/// free balances permissionlessly, so they are deployment by definition
/// (§8.4b).
public fun begin_quote_session<W: drop>(
    vault: &TradingVault,
    reg: &IntegrationRegistry,
    _witness: W,
): Session {
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(!is_risk_off(vault), errors::risk_off());
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
/// non-discretionary maintenance moves. Can never `take`; not
/// allowlist-gated for the same reason as `begin_force_session`.
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
/// session (and every session opened risk-off is forced).
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
/// address. Witness-gated AND allowlist-gated so junk objects can never
/// wedge appraisals (unclaimed transfers just sit unreceived).
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

/// Sweep in a Coin transferred to the vault's own object address;
/// joins straight into free balances.
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

/// Start a NAV computation. Identical to v1 plus a capital snapshot
/// (§3.7): a composed transaction cannot consume a waterfall that any
/// same-transaction deposit/fulfillment/accrual has since moved. The
/// escrowed curator commitment positions are shares, not assets — they
/// are never appraisal legs.
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
        capital_seq_snapshot: vault.capital_seq,
        external_pending: vault.external.is_some() && vault.external.borrow().exposure > 0,
    }
}

/// Value one non-accounting free balance via an oracle attestation.
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

/// Record one custodied position's value. Only the adapter that
/// custodied the position may value it.
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

/// Completeness + staleness gate, returns NAV.
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
        capital_seq_snapshot,
        external_pending,
    } = a;
    assert!(vault_id == object::id(vault), errors::wrong_vault());
    assert!(remaining_types.is_empty(), errors::appraisal_incomplete());
    assert!(!external_pending, errors::appraisal_incomplete());
    assert!(appraised_positions.length() == position_total, errors::appraisal_incomplete());
    // Nothing may have moved since begin (same-PTB mutations invalidate).
    assert!(position_total == vault.position_count, errors::appraisal_mismatch());
    assert!(types_snapshot == vault.asset_types, errors::appraisal_mismatch());
    assert!(mutation_seq_snapshot == vault.mutation_seq, errors::appraisal_mismatch());
    assert!(capital_seq_snapshot == vault.capital_seq, errors::appraisal_mismatch());
    events::emit_vault_appraised(vault_id, total_value, position_total);
    total_value
}

/// Permissionless mark refresh: validates like every other consume and
/// discards the NAV — the only effect is fresh mark events. Immutable,
/// so it can neither move value nor skew a snapshot.
public fun crank_appraisal(vault: &TradingVault, appraisal: Appraisal) {
    let _ = consume_appraisal(vault, appraisal);
}

/// Permissionless capital crank: consume a complete appraisal and run
/// the full capital sync — hurdle accrual, waterfall, risk-state
/// transition, commitment test. This is the keeper's cadence call; the
/// hurdle accrual cap makes its cadence a correctness obligation (§3.3,
/// §9.4).
public fun crank_capital(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    clock: &Clock,
) {
    let nav = consume_appraisal(vault, appraisal);
    let (_, _) = sync_capital(vault, cfg, nav, clock.timestamp_ms());
}

fun assert_attestation_fresh(cfg: &VaultProtocolConfig, att: &PriceAttestation, clock: &Clock) {
    let now = clock.timestamp_ms();
    let ts = price::timestamp_ms(att);
    // Future timestamps (skew) are fine.
    if (ts < now) {
        assert!(now - ts <= registry::max_price_age_ms(cfg), errors::price_stale());
    };
}

/// For adapters valuing their own holdings inside a position appraisal.
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

/// Register (or rotate) the vault's external account. Admin-gated.
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

/// Curator self-serve registration, authorized by an ed25519 attestation
/// from the protocol registrar. FIRST-SET-ONLY; limits capped below the
/// admin path's. Byte format identical to v1 (`tv_external_reg_v1`).
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

/// The exact bytes the registrar signs: domain tag ‖ vault id ‖ account.
public fun external_registration_message(vault_id: address, account: address): vector<u8> {
    let mut msg = EXTERNAL_REG_DOMAIN;
    msg.append(vault_id.to_bytes());
    msg.append(account.to_bytes());
    msg
}

/// Deregister the external account — only at zero exposure.
public fun clear_external_account(_: &AdminCap, vault: &mut TradingVault) {
    assert!(vault.external.is_some(), errors::external_not_configured());
    let ExternalAccount { exposure, .. } = vault.external.extract();
    assert!(exposure == 0, errors::external_exposure_open());
    events::emit_external_account_cleared(object::id(vault));
}

/// Curator-gated, budgeted release to the registered external account —
/// off-chain deployment is risk-increasing per se, so it ABORTS in every
/// risk-off state (§8.4b). Consumes a complete appraisal so limits bind
/// against true NAV and the capital state is fresh.
public fun release_external<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    amount: u64,
    clock: &Clock,
    ctx: &mut TxContext,
): u128 {
    assert_current_cap(vault, cap);
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    assert!(amount > 0, core_errors::zero_amount());
    assert!(vault.external.is_some(), errors::external_not_configured());
    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    let (_, _) = sync_capital(vault, cfg, nav, now);
    assert!(!is_risk_off(vault), errors::risk_off());

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
    nav
}

/// Repatriation: the registered account (and only it) pays
/// accounting-asset funds back, reducing exposure. Value inbound is
/// always allowed, in every state.
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

/// The external account's equity leg of an appraisal. Only the PINNED
/// oracle-adapter witness may record it, and only while allowlisted.
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

// ═══════════════════════ junior reset (§8.5) ═══════════════════════

/// Initiate the two-stage reset. Permissionless — eligibility is fully
/// objective: a complete appraisal must show active junior shares > 0,
/// junior NAV == 0, and total NAV < accrued senior claim. Records the
/// quoted terms (disclosure; the binding minimum is recomputed at
/// execution) and the execution deadline: no earlier than seven days of
/// persistent impairment AND seven days of public notice.
public fun propose_junior_reset(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    clock: &Clock,
) {
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(capital::is_tranched(&vault.capital), errors::wrong_tranche());
    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    let (_, junior_nav) = sync_capital(vault, cfg, nav, now);

    let claim = capital::senior_claim(&vault.book);
    assert!(
        capital::junior_shares(&vault.book) > 0 && junior_nav == 0 && nav < claim,
        errors::reset_not_eligible(),
    );
    let required = capital::min_reset_deposit(
        nav,
        claim,
        capital::target_junior_bps(&vault.capital),
    );
    assert!(required <= (std::u64::max_value!() as u128), errors::reset_not_eligible());
    capital::propose_reset(&mut vault.book, now, nav, claim, required as u64);
    vault.capital_seq = vault.capital_seq + 1;
    let (old_gen, proposed_at, executable_at, _, _, _) =
        capital::reset_proposal_fields(&vault.book);
    events::emit_junior_reset_proposed(
        object::id(vault),
        old_gen,
        proposed_at,
        executable_at,
        nav,
        claim,
        claim - nav,
        required as u64,
    );
}

/// Atomic revalidation and funding (§8.5.4–6). Permissionless for any
/// issuance-whitelisted user once the objective conditions, seasoning,
/// notice, fresh appraisal, and minimum deposit hold — no discretionary
/// seizure, and no reset without new money. The deposit must be the
/// accounting asset. `N` and `C` are re-derived from THIS appraisal.
/// Returns the recapitalizer's genesis junior position of the new
/// generation. The old generation becomes a permanent zero-value claim;
/// the senior claim is not written down.
public fun execute_junior_reset<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    wl: &Whitelist,
    appraisal: Appraisal,
    funds: Coin<T>,
    clock: &Clock,
    ctx: &mut TxContext,
): VaultPosition {
    whitelist::assert_ingress_allowed(wl, ctx.sender());
    assert!(!registry::is_paused(cfg), errors::protocol_paused());
    assert!(vault.state == VaultState::Open, errors::vault_not_open());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    // The sync re-checks eligibility: recovery cancels the proposal.
    let (_, junior_nav) = sync_capital(vault, cfg, nav, now);
    assert!(capital::has_reset_proposal(&vault.book), errors::reset_not_ready());
    assert!(now >= capital::reset_executable_at_ms(&vault.book), errors::reset_not_ready());
    let claim = capital::senior_claim(&vault.book);
    assert!(
        capital::junior_shares(&vault.book) > 0 && junior_nav == 0 && nav < claim,
        errors::reset_not_eligible(),
    );

    let deposit_value = funds.value();
    assert!(deposit_value > 0, core_errors::zero_amount());
    let required = capital::min_reset_deposit(
        nav,
        claim,
        capital::target_junior_bps(&vault.capital),
    );
    assert!(
        (deposit_value as u128) >= required && nav + (deposit_value as u128) > claim,
        errors::reset_deposit_insufficient(),
    );

    put_balance_internal<T>(vault, funds.into_balance());
    // Genesis mint of the new generation: standard virtual-offset math
    // from a zero-supply, zero-NAV book — the recapitalizer owns the
    // whole generation, whose NAV is exactly `N + D − C` after the
    // deposit's first `C − N` units cure the senior deficit.
    let minted = fees::shares_for_value(deposit_value, 0, 0, SHARE_OFFSET);
    let old_generation = capital::active_junior_generation(&vault.book);
    capital::execute_reset(&mut vault.book, minted);
    vault.capital_seq = vault.capital_seq + 1;
    let new_generation = capital::active_junior_generation(&vault.book);
    // The escrowed curator commitment (if any) was old-generation junior
    // — wiped. Risk-increasing activity stays disabled until the curator
    // funds a compliant new-generation commitment (§8.5.7).
    vault.curator_commitment_breached = true;

    let position = vault_position::mint(
        object::id(vault),
        capital::tranche_from_code(2),
        minted,
        deposit_value,
        now + vault.config.lockup_ms,
        new_generation,
        ctx,
    );
    let post_junior_nav = nav + (deposit_value as u128) - claim;
    events::emit_junior_reset_executed(
        object::id(vault),
        old_generation,
        new_generation,
        ctx.sender(),
        deposit_value,
        post_junior_nav,
        object::id(&position),
    );
    position
}

/// Cleanup for a wiped-generation junior position: destroys the NFT at
/// its permanent zero value. Permissionless for the holder (they own the
/// object); it can never become active again (§8.5).
public fun burn_wiped_position(vault: &TradingVault, position: VaultPosition) {
    assert!(vault_position::vault_id(&position) == object::id(vault), errors::wrong_position_vault());
    let wiped = capital::is_junior(&vault_position::tranche(&position))
        && vault_position::capital_generation(&position)
            < capital::active_junior_generation(&vault.book);
    assert!(wiped, errors::position_not_wiped());
    let (position_id, _, shares, _, _, generation) = vault_position::consume(position);
    events::emit_wiped_position_burned(object::id(vault), position_id, generation, shares);
}

// ═══════════════════ curator commitment escrow (§8.6) ═══════════════════

/// Split `shares` out of the current cap's escrowed commitment position
/// into an ordinary, freely transferable position NFT. Consumes a fresh
/// appraisal: while Open the release must leave the remaining marked
/// commitment at or above the floor, and is blocked outright in any
/// risk-off state. While Closing the floor is waived (exits are the
/// point). Pass `shares == 0` to release the ENTIRE escrowed position.
public fun release_commitment(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    shares: u128,
    clock: &Clock,
    ctx: &mut TxContext,
): VaultPosition {
    assert_current_cap(vault, cap);
    assert!(vault.state != VaultState::Closed, errors::vault_not_closed());
    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    let (_, junior_nav) = sync_capital(vault, cfg, nav, now);
    if (vault.state == VaultState::Open) {
        assert!(!is_risk_off(vault), errors::risk_off());
    };

    let tranche_code = capital::tranche_code(&commitment_tranche(vault));
    let key = CommitmentKey { cap_id: vault.curator_cap_id, tranche: tranche_code };
    assert!(dof::exists(&vault.id, key), errors::commitment_missing());

    let take_all = {
        let p: &VaultPosition = dof::borrow(&vault.id, key);
        shares == 0 || shares == vault_position::shares(p)
    };
    let released = if (take_all) {
        let p: VaultPosition = dof::remove(&mut vault.id, key);
        p
    } else {
        let p: &mut VaultPosition = dof::borrow_mut(&mut vault.id, key);
        vault_position::split(p, shares, ctx)
    };

    // Floor test on what remains (Open only): marked value of the
    // remaining escrow must cover the commitment floor.
    if (vault.state == VaultState::Open) {
        vault.curator_commitment_breached = commitment_breached(vault, cfg, nav, junior_nav);
        assert!(!vault.curator_commitment_breached, errors::curator_floor());
    };

    events::emit_commitment_released(
        object::id(vault),
        vault.curator_cap_id,
        object::id(&released),
        vault_position::shares(&released),
        vault_position::cost_basis(&released),
    );
    released
}

/// Once the vault is Closed and settled, the escrowed commitment is just
/// another claim on the pool: hand it back to the cap holder so it can
/// be redeemed like any position. No floor, no appraisal — NAV is
/// frozen.
public fun withdraw_commitment_settled(
    vault: &mut TradingVault,
    cap: &CuratorCap,
): VaultPosition {
    assert!(cap.vault_id == object::id(vault), errors::wrong_vault());
    assert!(vault.state == VaultState::Closed, errors::vault_not_closed());
    assert!(vault.settlement.is_some(), errors::not_settled());
    // Any cap (current or rotated-out claim ticket) may pull ITS OWN
    // escrowed positions.
    let cap_id = object::id(cap);
    let mut code = 0u8;
    while (code < 3) {
        let key = CommitmentKey { cap_id, tranche: code };
        if (dof::exists(&vault.id, key)) {
            let p: VaultPosition = dof::remove(&mut vault.id, key);
            events::emit_commitment_released(
                object::id(vault),
                cap_id,
                object::id(&p),
                vault_position::shares(&p),
                vault_position::cost_basis(&p),
            );
            return p
        };
        code = code + 1;
    };
    abort errors::commitment_missing()
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

/// Permissionless: Closing → Closed once every position is gone, live
/// external exposure is repatriated, and only the accounting asset
/// remains. The settlement snapshot (below) then freezes entitlements.
public fun finalize_close(vault: &mut TradingVault) {
    assert!(vault.state == VaultState::Closing, errors::vault_not_closing());
    assert!(vault.position_count == 0, errors::positions_open());
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

// ═══════════════════ terminal settlement pool (§8.7) ═══════════════════

/// One-time, permissionless settlement snapshot: consume a final
/// complete appraisal, run the waterfall ONCE, and freeze each tranche's
/// entitlement — senior first, pro rata within a tranche if assets are
/// short, wiped generations at zero. Every outstanding share (wallet
/// positions, escrowed commitments, queued requests) redeems against
/// the pool at any later time; NAV is frozen, so redemption is a pure
/// lookup and balance split, and late redemption costs nobody anything.
public fun snapshot_settlement(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    appraisal: Appraisal,
    clock: &Clock,
) {
    assert!(vault.state == VaultState::Closed, errors::vault_not_closed());
    assert!(vault.settlement.is_none(), errors::already_settled());
    let now = clock.timestamp_ms();
    let nav = consume_appraisal(vault, appraisal);
    let (senior_nav, junior_nav) = sync_capital(vault, cfg, nav, now);
    assert!(nav <= (std::u64::max_value!() as u128), errors::config_invalid());

    let pool = SettlementPool {
        senior_pool: senior_nav as u64,
        senior_supply: capital::senior_shares(&vault.book),
        junior_pool: junior_nav as u64,
        junior_supply: capital::junior_shares(&vault.book),
        active_junior_generation: capital::active_junior_generation(&vault.book),
        curator_fees_accrued: 0,
    };
    events::emit_settlement_snapshot(
        object::id(vault),
        nav,
        pool.senior_pool,
        pool.senior_supply,
        pool.junior_pool,
        pool.junior_supply,
        pool.active_junior_generation,
    );
    vault.settlement.fill(pool);
    vault.capital_seq = vault.capital_seq + 1;
}

/// Redeem a wallet-held position directly against the pool — no queue,
/// no appraisal, no keeper. Performance fees crystallize here exactly as
/// at fulfillment; the curator's net accrues as a cash claim on the pool
/// (share mints are impossible after the snapshot).
public fun redeem_settled_position<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    position: VaultPosition,
    ctx: &mut TxContext,
) {
    assert!(vault_position::vault_id(&position) == object::id(vault), errors::wrong_position_vault());
    assert!(vault.state == VaultState::Closed, errors::vault_not_closed());
    assert!(vault.settlement.is_some(), errors::not_settled());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let (position_id, tranche, shares, basis, _, generation) = vault_position::consume(position);
    pay_settlement<T>(
        vault,
        cfg,
        treasury,
        position_id,
        false,
        0,
        ctx.sender(),
        &tranche,
        generation,
        shares,
        basis,
        ctx,
    );
}

/// Settle an outstanding queued request from the pool at the snapshot
/// entitlement (its position was already consumed at request time).
/// Permissionless — order no longer matters once NAV is frozen.
public fun settle_queued_request<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    global_seq: u64,
    ctx: &mut TxContext,
) {
    assert!(vault.state == VaultState::Closed, errors::vault_not_closed());
    assert!(vault.settlement.is_some(), errors::not_settled());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    assert!(vault.requests.contains(global_seq), errors::request_missing());
    let WithdrawRequest {
        position_id,
        recipient,
        tranche,
        capital_generation,
        shares,
        basis,
        payout_asset: _,
        requested_at_ms: _,
        lane_idx,
    } = vault.requests.remove(global_seq);
    let lane = if (lane_code_of(&tranche) == LANE_JUNIOR) { &mut vault.junior_lane } else {
        &mut vault.senior_lane
    };
    lane.entries.remove(lane_idx);
    pay_settlement<T>(
        vault,
        cfg,
        treasury,
        position_id,
        true,
        global_seq,
        recipient,
        &tranche,
        capital_generation,
        shares,
        basis,
        ctx,
    );
}

fun pay_settlement<T>(
    vault: &mut TradingVault,
    cfg: &VaultProtocolConfig,
    treasury: &mut Treasury,
    position_id: ID,
    from_queue: bool,
    global_seq: u64,
    recipient: address,
    tranche: &Tranche,
    generation: u64,
    shares: u128,
    basis: u64,
    ctx: &mut TxContext,
) {
    let (entitlement, gross_fee, protocol_cut, curator_net) = {
        let pool = vault.settlement.borrow();
        let wiped = capital::is_junior(tranche) && generation < pool.active_junior_generation;
        let (t_pool, t_supply) = if (capital::is_senior(tranche)) {
            (pool.senior_pool, pool.senior_supply)
        } else { (pool.junior_pool, pool.junior_supply) };
        let entitlement = if (wiped || t_supply == 0) { 0 } else {
            ((t_pool as u256) * (shares as u256) / (t_supply as u256)) as u64
        };
        let (_, gross_fee, protocol_cut, curator_net) = fees::crystallize(
            entitlement,
            basis,
            vault.config.curator_fee_bps,
            registry::protocol_fee_bps(cfg),
        );
        (entitlement, gross_fee, protocol_cut, curator_net)
    };
    if (curator_net > 0) {
        let pool = vault.settlement.borrow_mut();
        pool.curator_fees_accrued = pool.curator_fees_accrued + curator_net;
    };
    if (protocol_cut > 0) {
        treasury::deposit_balance(treasury, take_balance_internal<T>(vault, protocol_cut));
    };
    let payout = entitlement - gross_fee;
    if (payout > 0) {
        transfer::public_transfer(
            coin::from_balance(take_balance_internal<T>(vault, payout), ctx),
            recipient,
        );
    };
    events::emit_settlement_redeemed(
        object::id(vault),
        position_id,
        from_queue,
        global_seq,
        recipient,
        capital::tranche_code(tranche),
        generation,
        shares,
        entitlement,
        basis,
        gross_fee,
        protocol_cut,
        curator_net,
        payout,
    );
}

/// Pay out the curator performance fees accrued from settlement
/// redemptions. Current-cap-gated.
#[allow(lint(self_transfer))]
public fun claim_settlement_curator_fees<T>(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    assert!(vault.settlement.is_some(), errors::not_settled());
    assert!(
        type_name::with_defining_ids<T>() == vault.config.accounting_asset,
        errors::deposit_asset_mismatch(),
    );
    let amount = {
        let pool = vault.settlement.borrow_mut();
        let a = pool.curator_fees_accrued;
        pool.curator_fees_accrued = 0;
        a
    };
    assert!(amount > 0, core_errors::zero_amount());
    transfer::public_transfer(
        coin::from_balance(take_balance_internal<T>(vault, amount), ctx),
        ctx.sender(),
    );
    events::emit_settlement_curator_fees_claimed(object::id(vault), vault.curator_cap_id, amount);
}

// ═══════════════════════════ rotation ═══════════════════════════

/// Rotate the curator role. The old cap's escrowed commitment positions
/// are released to the OUTGOING holder as ordinary, fully transferable
/// claim tickets (§2.2); the incoming cap must fund a new escrowed
/// commitment before discretionary sessions resume — the commitment
/// breach flag is set pessimistically until then.
#[allow(lint(self_transfer))]
public fun rotate_curator_by_curator(
    vault: &mut TradingVault,
    cap: &CuratorCap,
    recipient: address,
    ctx: &mut TxContext,
) {
    assert_current_cap(vault, cap);
    let old_cap_id = vault.curator_cap_id;
    // Hand every escrowed commitment slot back to the outgoing curator.
    let mut code = 0u8;
    while (code < 3) {
        let key = CommitmentKey { cap_id: old_cap_id, tranche: code };
        if (dof::exists(&vault.id, key)) {
            let p: VaultPosition = dof::remove(&mut vault.id, key);
            events::emit_commitment_released(
                object::id(vault),
                old_cap_id,
                object::id(&p),
                vault_position::shares(&p),
                vault_position::cost_basis(&p),
            );
            transfer::public_transfer(p, ctx.sender());
        };
        code = code + 1;
    };
    let new_cap = CuratorCap { id: object::new(ctx), vault_id: object::id(vault) };
    let new_cap_id = object::id(&new_cap);
    vault.curator_cap_id = new_cap_id;
    if (vault.state == VaultState::Open) {
        vault.curator_commitment_breached = true;
    };
    events::emit_curator_rotated(object::id(vault), old_cap_id, new_cap_id, recipient);
    transfer::public_transfer(new_cap, recipient);
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

/// Package-private outflow for the first-party `vault_mm` module. Gated
/// on the capital risk states here — the single choke point for the
/// quote-collateral path (§8.4b).
public(package) fun release_for_mm<T>(vault: &mut TradingVault, amount: u64): Balance<T> {
    assert!(!is_risk_off(vault), errors::risk_off());
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

public fun is_settled(vault: &TradingVault): bool { vault.settlement.is_some() }

public fun accounting_asset(vault: &TradingVault): TypeName { vault.config.accounting_asset }

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

/// Fields of the pending request at `global_seq`: (position_id,
/// recipient, tranche_code, capital_generation, shares, basis,
/// payout_asset, requested_at_ms, lane_code).
public fun queue_request(
    vault: &TradingVault,
    global_seq: u64,
): (ID, address, u8, u64, u128, u64, TypeName, u64, u8) {
    assert!(vault.requests.contains(global_seq), errors::request_missing());
    let r = vault.requests.borrow(global_seq);
    (
        r.position_id,
        r.recipient,
        capital::tranche_code(&r.tranche),
        r.capital_generation,
        r.shares,
        r.basis,
        r.payout_asset,
        r.requested_at_ms,
        lane_code_of(&r.tranche),
    )
}

public fun has_request(vault: &TradingVault, global_seq: u64): bool {
    vault.requests.contains(global_seq)
}

/// (head, tail) of the lane: 0 = senior, anything else = junior.
public fun lane_bounds(vault: &TradingVault, lane: u8): (u64, u64) {
    if (lane == LANE_SENIOR) {
        (vault.senior_lane.head, vault.senior_lane.tail)
    } else {
        (vault.junior_lane.head, vault.junior_lane.tail)
    }
}

/// The global sequence at `idx` in a lane's entry table (aborts on
/// gaps/missing).
public fun lane_entry(vault: &TradingVault, lane: u8, idx: u64): u64 {
    if (lane == LANE_SENIOR) {
        *vault.senior_lane.entries.borrow(idx)
    } else {
        *vault.junior_lane.entries.borrow(idx)
    }
}

public fun next_global_seq(vault: &TradingVault): u64 { vault.next_global_seq }

public fun share_offset(): u128 { SHARE_OFFSET }

public fun total_shares(vault: &TradingVault): u128 {
    capital::senior_shares(&vault.book) + capital::junior_shares(&vault.book)
}

public fun position_count(vault: &TradingVault): u64 { vault.position_count }

public fun free_balance_of<T>(vault: &TradingVault): u64 { free_balance_value<T>(vault) }

public fun curator_cap_id(vault: &TradingVault): ID { vault.curator_cap_id }

public fun creator(vault: &TradingVault): address { vault.creator }

public fun lockup_ms(vault: &TradingVault): u64 { vault.config.lockup_ms }

public fun curator_fee_bps(vault: &TradingVault): u64 { vault.config.curator_fee_bps }

public fun unwind_grace_ms(vault: &TradingVault): u64 { vault.config.unwind_grace_ms }

public fun mm_release_enabled(vault: &TradingVault): bool { vault.config.mm_release_enabled }

public fun pending_withdrawals(vault: &TradingVault): u64 {
    vault.requests.length()
}

public fun capital_structure(vault: &TradingVault): &CapitalStructure { &vault.capital }

public fun book(vault: &TradingVault): &TrancheBook { &vault.book }

public fun terms_version(vault: &TradingVault): u64 { vault.terms_version }

public fun spec_hash(vault: &TradingVault): &vector<u8> { &vault.spec_hash }

public fun curator_commitment_breached(vault: &TradingVault): bool {
    vault.curator_commitment_breached
}

/// (senior_pool, senior_supply, junior_pool, junior_supply, generation,
/// curator_fees_accrued). Aborts before the snapshot.
public fun settlement_pool(vault: &TradingVault): (u64, u128, u64, u128, u64, u64) {
    assert!(vault.settlement.is_some(), errors::not_settled());
    let p = vault.settlement.borrow();
    (
        p.senior_pool,
        p.senior_supply,
        p.junior_pool,
        p.junior_supply,
        p.active_junior_generation,
        p.curator_fees_accrued,
    )
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

/// Does an escrowed commitment position exist for `cap_id` in the
/// commitment tranche, and its (shares, basis, generation) if so.
public fun commitment_of(vault: &TradingVault, cap_id: ID): (bool, u128, u64, u64) {
    let key = CommitmentKey {
        cap_id,
        tranche: capital::tranche_code(&commitment_tranche(vault)),
    };
    if (!dof::exists(&vault.id, key)) { return (false, 0, 0, 0) };
    let p: &VaultPosition = dof::borrow(&vault.id, key);
    (
        true,
        vault_position::shares(p),
        vault_position::cost_basis(p),
        vault_position::capital_generation(p),
    )
}

/// Immutable access to a custodied position (e.g. for appraisal reads).
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
