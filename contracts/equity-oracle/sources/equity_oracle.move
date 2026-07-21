/// Equity oracle for trading-vault external accounts
/// (docs/mm-bot-v2/03-bluefin-integration-plan.md §3a,
/// 04-deepbook-margin-integration-plan.md §3a — venue-neutral by design):
/// keeper-posted account equity, consumed by vault appraisals through
/// `vault::record_external_equity` with this package's allowlisted
/// `EquityOracle` witness.
///
/// The posted value is operator input, so it is guardrailed ON-CHAIN, the
/// same posture as every other operator-attested input in the protocol:
///
///   • poster allowlist — only admin-registered keeper addresses post;
///   • min update interval — a compromised poster cannot walk the value
///     far by rapid-fire updates;
///   • max delta per update (bps vs the previous value) — each step is
///     bounded, so drift speed is `max_delta_bps` per `min_interval_ms`;
///   • staleness backstop — appraisals refuse entries older than
///     `max_age_ms` (on top of the vault's own attestation-age backstop).
///
/// A zeroed or diverged entry is re-anchored by the admin (`seed_equity`),
/// which bypasses the delta guardrail — registering an account and
/// re-seeding after venue rotation are governance acts, not keeper acts.
///
/// The trustless sibling for readable venues (e.g. DeepBook Margin, whose
/// `MarginManager` is a readable shared object) is a computed adapter that
/// derives equity from venue state + Pyth legs inside the appraisal PTB
/// itself; it plugs into the same `record_external_equity` surface with
/// its own witness and needs none of these guardrails.
module equity_oracle::equity_oracle;

use sui::clock::Clock;
use sui::event;
use sui::table::{Self, Table};
use sui::vec_set::{Self, VecSet};

use options_core::admin::AdminCap;

use trading_vault::registry::OracleRegistry;
use trading_vault::vault::{Self, Appraisal, TradingVault};

const DEFAULT_MAX_AGE_MS: u64 = 300_000; // 5 min
const DEFAULT_MAX_DELTA_BPS: u64 = 2_000; // 20% per update
const DEFAULT_MIN_INTERVAL_MS: u64 = 60_000; // 1 min

const BPS_DENOM: u128 = 10_000;

const E_NOT_POSTER: u64 = 1;
const E_NOT_SEEDED: u64 = 2;
const E_TOO_SOON: u64 = 3;
const E_DELTA_TOO_LARGE: u64 = 4;
const E_STALE: u64 = 5;
const E_CONFIG_INVALID: u64 = 6;

/// Witness minted only by this module's record path; allowlist it in the
/// `OracleRegistry` and pin it on the vault via `set_external_account`.
public struct EquityOracle has drop {}

public struct EquityEntry has copy, drop, store {
    equity: u64,
    updated_at_ms: u64,
}

/// Shared, admin-governed book of per-vault external-account equity.
public struct EquityBook has key {
    id: UID,
    entries: Table<ID, EquityEntry>,
    posters: VecSet<address>,
    max_age_ms: u64,
    max_delta_bps: u64,
    min_interval_ms: u64,
}

public struct EquityPosted has copy, drop {
    vault_id: ID,
    poster: address,
    equity: u64,
    previous: u64,
    seeded: bool,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(EquityBook {
        id: object::new(ctx),
        entries: table::new(ctx),
        posters: vec_set::empty(),
        max_age_ms: DEFAULT_MAX_AGE_MS,
        max_delta_bps: DEFAULT_MAX_DELTA_BPS,
        min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun add_poster(_: &AdminCap, book: &mut EquityBook, poster: address) {
    book.posters.insert(poster);
}

public fun remove_poster(_: &AdminCap, book: &mut EquityBook, poster: address) {
    book.posters.remove(&poster);
}

/// Anchor (or re-anchor) a vault's entry, bypassing the delta/interval
/// guardrails — registration, venue rotation, and divergence recovery are
/// governance acts.
public fun seed_equity(
    _: &AdminCap,
    book: &mut EquityBook,
    vault_id: ID,
    equity: u64,
    clock: &Clock,
    ctx: &TxContext,
) {
    let entry = EquityEntry { equity, updated_at_ms: clock.timestamp_ms() };
    let previous = if (book.entries.contains(vault_id)) {
        let prev = book.entries.borrow_mut(vault_id);
        let old = prev.equity;
        *prev = entry;
        old
    } else {
        book.entries.add(vault_id, entry);
        0
    };
    event::emit(EquityPosted {
        vault_id,
        poster: ctx.sender(),
        equity,
        previous,
        seeded: true,
    });
}

public fun remove_entry(_: &AdminCap, book: &mut EquityBook, vault_id: ID) {
    let EquityEntry { equity: _, updated_at_ms: _ } = book.entries.remove(vault_id);
}

public fun set_max_age_ms(_: &AdminCap, book: &mut EquityBook, ms: u64) {
    assert!(ms > 0, E_CONFIG_INVALID);
    book.max_age_ms = ms;
}

public fun set_max_delta_bps(_: &AdminCap, book: &mut EquityBook, bps: u64) {
    assert!(bps > 0, E_CONFIG_INVALID);
    book.max_delta_bps = bps;
}

public fun set_min_interval_ms(_: &AdminCap, book: &mut EquityBook, ms: u64) {
    book.min_interval_ms = ms;
}

// ═══════════════════════════════ posting ═══════════════════════════════

/// Keeper path: update a seeded entry within the guardrails. A previous
/// value of zero cannot be moved by a poster at all (bps-of-zero is zero)
/// — recovery from a zeroed mark goes through `seed_equity`.
public fun post_equity(
    book: &mut EquityBook,
    vault_id: ID,
    equity: u64,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(book.posters.contains(&ctx.sender()), E_NOT_POSTER);
    assert!(book.entries.contains(vault_id), E_NOT_SEEDED);
    let now = clock.timestamp_ms();
    let max_delta_bps = book.max_delta_bps;
    let min_interval_ms = book.min_interval_ms;
    let entry = book.entries.borrow_mut(vault_id);
    assert!(now >= entry.updated_at_ms + min_interval_ms, E_TOO_SOON);
    let previous = entry.equity;
    let delta = if (equity > previous) { equity - previous } else { previous - equity };
    assert!(
        (delta as u128) * BPS_DENOM <= (previous as u128) * (max_delta_bps as u128),
        E_DELTA_TOO_LARGE,
    );
    entry.equity = equity;
    entry.updated_at_ms = now;
    event::emit(EquityPosted {
        vault_id,
        poster: ctx.sender(),
        equity,
        previous,
        seeded: false,
    });
}

// ═══════════════════════════════ appraisal ═══════════════════════════════

/// Record the vault's external-equity leg from the book. Composable into
/// any appraisal PTB (deposits, fulfillment cranks, releases); aborts if
/// the entry is missing or older than `max_age_ms`.
public fun record(
    vault: &TradingVault,
    book: &EquityBook,
    reg: &OracleRegistry,
    a: &mut Appraisal,
    clock: &Clock,
) {
    let vault_id = object::id(vault);
    assert!(book.entries.contains(vault_id), E_NOT_SEEDED);
    let entry = book.entries.borrow(vault_id);
    let now = clock.timestamp_ms();
    if (entry.updated_at_ms < now) {
        assert!(now - entry.updated_at_ms <= book.max_age_ms, E_STALE);
    };
    vault::record_external_equity(vault, reg, a, EquityOracle {}, entry.equity);
}

// ══════════════════════════════ getters ══════════════════════════════

public fun has_entry(book: &EquityBook, vault_id: ID): bool {
    book.entries.contains(vault_id)
}

/// (equity, updated_at_ms).
public fun entry(book: &EquityBook, vault_id: ID): (u64, u64) {
    let e = book.entries.borrow(vault_id);
    (e.equity, e.updated_at_ms)
}

public fun is_poster(book: &EquityBook, poster: address): bool {
    book.posters.contains(&poster)
}

public fun max_age_ms(book: &EquityBook): u64 { book.max_age_ms }

public fun max_delta_bps(book: &EquityBook): u64 { book.max_delta_bps }

public fun min_interval_ms(book: &EquityBook): u64 { book.min_interval_ms }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
