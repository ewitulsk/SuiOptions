/// Attested realized-vol book for option premium marks (SO-299
/// follow-up: `options_oracle` premium mark-to-market).
///
/// One annualized vol per UNDERLYING asset type, posted by the keeper
/// from Pyth benchmark history. The posted value is operator input, so
/// it is guardrailed ON-CHAIN with the same posture as every other
/// operator-attested input (mirrors `equity_oracle`):
///
///   • poster allowlist — only admin-registered keeper addresses post;
///   • min update interval — rapid-fire walking is bounded;
///   • max delta per update (bps of previous) — each step is bounded;
///   • hard vol ceiling — no posted value can exceed `max_vol_bps`;
///   • staleness — a stale entry contributes NO extrinsic value
///     (marks degrade to intrinsic-only instead of wedging appraisals).
///
/// A zeroed or diverged entry is re-anchored by the admin (`seed_vol`),
/// which bypasses the delta/interval guardrails. A missing entry means
/// intrinsic-only marks for that underlying — the pre-vol behavior.
module options_adapter::vol_book;

use std::type_name::TypeName;
use sui::clock::Clock;
use sui::event;
use sui::table::{Self, Table};
use sui::vec_set::{Self, VecSet};

use options_core::admin::AdminCap;

const DEFAULT_MAX_AGE_MS: u64 = 3_600_000; // 1h — realized vol moves slowly
const DEFAULT_MAX_DELTA_BPS: u64 = 2_000; // 20% per update
const DEFAULT_MIN_INTERVAL_MS: u64 = 60_000; // 1 min
/// 400% annualized — nothing sane exceeds this; a compromised poster
/// cannot inflate marks past it even by walking.
const DEFAULT_MAX_VOL_BPS: u64 = 40_000;

const BPS_DENOM: u128 = 10_000;

const E_NOT_POSTER: u64 = 1;
const E_NOT_SEEDED: u64 = 2;
const E_TOO_SOON: u64 = 3;
const E_DELTA_TOO_LARGE: u64 = 4;
const E_CONFIG_INVALID: u64 = 5;
const E_VOL_TOO_LARGE: u64 = 6;

public struct VolEntry has copy, drop, store {
    /// Annualized vol in basis points (8_000 = 80%).
    vol_bps: u64,
    updated_at_ms: u64,
}

/// Shared, admin-governed book of per-underlying annualized vol.
public struct VolBook has key {
    id: UID,
    entries: Table<TypeName, VolEntry>,
    posters: VecSet<address>,
    max_age_ms: u64,
    max_delta_bps: u64,
    min_interval_ms: u64,
    max_vol_bps: u64,
}

public struct VolPosted has copy, drop {
    underlying: TypeName,
    poster: address,
    vol_bps: u64,
    previous: u64,
    seeded: bool,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(VolBook {
        id: object::new(ctx),
        entries: table::new(ctx),
        posters: vec_set::empty(),
        max_age_ms: DEFAULT_MAX_AGE_MS,
        max_delta_bps: DEFAULT_MAX_DELTA_BPS,
        min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
        max_vol_bps: DEFAULT_MAX_VOL_BPS,
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun add_poster(_: &AdminCap, book: &mut VolBook, poster: address) {
    book.posters.insert(poster);
}

public fun remove_poster(_: &AdminCap, book: &mut VolBook, poster: address) {
    book.posters.remove(&poster);
}

/// Anchor (or re-anchor) an underlying's vol, bypassing the
/// delta/interval guardrails — seeding and divergence recovery are
/// governance acts.
public fun seed_vol(
    _: &AdminCap,
    book: &mut VolBook,
    underlying: TypeName,
    vol_bps: u64,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(vol_bps <= book.max_vol_bps, E_VOL_TOO_LARGE);
    let entry = VolEntry { vol_bps, updated_at_ms: clock.timestamp_ms() };
    let previous = if (book.entries.contains(underlying)) {
        let prev = book.entries.borrow_mut(underlying);
        let old = prev.vol_bps;
        *prev = entry;
        old
    } else {
        book.entries.add(underlying, entry);
        0
    };
    event::emit(VolPosted {
        underlying,
        poster: ctx.sender(),
        vol_bps,
        previous,
        seeded: true,
    });
}

public fun remove_entry(_: &AdminCap, book: &mut VolBook, underlying: TypeName) {
    let VolEntry { vol_bps: _, updated_at_ms: _ } = book.entries.remove(underlying);
}

public fun set_max_age_ms(_: &AdminCap, book: &mut VolBook, ms: u64) {
    assert!(ms > 0, E_CONFIG_INVALID);
    book.max_age_ms = ms;
}

public fun set_max_delta_bps(_: &AdminCap, book: &mut VolBook, bps: u64) {
    assert!(bps > 0, E_CONFIG_INVALID);
    book.max_delta_bps = bps;
}

public fun set_min_interval_ms(_: &AdminCap, book: &mut VolBook, ms: u64) {
    book.min_interval_ms = ms;
}

public fun set_max_vol_bps(_: &AdminCap, book: &mut VolBook, bps: u64) {
    assert!(bps > 0, E_CONFIG_INVALID);
    book.max_vol_bps = bps;
}

// ═══════════════════════════════ posting ═══════════════════════════════

/// Keeper path: update a seeded entry within the guardrails. A previous
/// value of zero cannot be moved by a poster (bps-of-zero is zero) —
/// recovery goes through `seed_vol`.
public fun post_vol(
    book: &mut VolBook,
    underlying: TypeName,
    vol_bps: u64,
    clock: &Clock,
    ctx: &TxContext,
) {
    assert!(book.posters.contains(&ctx.sender()), E_NOT_POSTER);
    assert!(book.entries.contains(underlying), E_NOT_SEEDED);
    assert!(vol_bps <= book.max_vol_bps, E_VOL_TOO_LARGE);
    let now = clock.timestamp_ms();
    let max_delta_bps = book.max_delta_bps;
    let min_interval_ms = book.min_interval_ms;
    let entry = book.entries.borrow_mut(underlying);
    assert!(now >= entry.updated_at_ms + min_interval_ms, E_TOO_SOON);
    let previous = entry.vol_bps;
    let delta = if (vol_bps > previous) { vol_bps - previous } else { previous - vol_bps };
    assert!(
        (delta as u128) * BPS_DENOM <= (previous as u128) * (max_delta_bps as u128),
        E_DELTA_TOO_LARGE,
    );
    entry.vol_bps = vol_bps;
    entry.updated_at_ms = now;
    event::emit(VolPosted {
        underlying,
        poster: ctx.sender(),
        vol_bps,
        previous,
        seeded: false,
    });
}

// ══════════════════════════════ reading ══════════════════════════════

/// The current vol for `underlying`, or 0 when missing or stale —
/// consumers degrade to intrinsic-only marks rather than aborting.
public fun current_vol_bps(book: &VolBook, underlying: TypeName, clock: &Clock): u64 {
    if (!book.entries.contains(underlying)) {
        return 0
    };
    let entry = book.entries.borrow(underlying);
    let now = clock.timestamp_ms();
    if (entry.updated_at_ms < now && now - entry.updated_at_ms > book.max_age_ms) {
        return 0
    };
    entry.vol_bps
}

// ══════════════════════════════ getters ══════════════════════════════

public fun has_entry(book: &VolBook, underlying: TypeName): bool {
    book.entries.contains(underlying)
}

/// (vol_bps, updated_at_ms).
public fun entry(book: &VolBook, underlying: TypeName): (u64, u64) {
    let e = book.entries.borrow(underlying);
    (e.vol_bps, e.updated_at_ms)
}

public fun is_poster(book: &VolBook, poster: address): bool {
    book.posters.contains(&poster)
}

public fun max_age_ms(book: &VolBook): u64 { book.max_age_ms }

public fun max_delta_bps(book: &VolBook): u64 { book.max_delta_bps }

public fun min_interval_ms(book: &VolBook): u64 { book.min_interval_ms }

public fun max_vol_bps(book: &VolBook): u64 { book.max_vol_bps }

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
