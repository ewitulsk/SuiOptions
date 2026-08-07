/// Per-market `SettlementRegistry` (spec §4.5): fill accounting keyed by
/// order digest, salt watermarks, pause flag, fee config and fee vaults.
///
/// One shared registry per trading pair keeps shared-object contention
/// bounded: fills on SUI/USDC never serialize behind fills on WAL/USDC. The
/// registry is typed by the pair, which closes the "same struct, different
/// token" spoofing hole at the type level.
module exchange::registry;

use sui::balance::{Self, Balance};
use sui::clock::Clock;
use sui::event;
use sui::table::{Self, Table};
use exchange::admin::AdminCap;
use exchange::order;

// === Errors ===

const EFeeTooHigh: u64 = 1;
const ESameToken: u64 = 2;
const EOverfill: u64 = 3;
const EZeroTickOrSize: u64 = 4;

/// Hard-coded protocol fee ceiling (bps) — a second belt under makers'
/// signed `max_fee_bps` caps (§4.9).
const MAX_FEE_BPS: u64 = 50;

/// Fill/cancel entries become garbage-collectable this long past expiry.
const GC_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Cumulative fill state per order digest. Doubles as the cancellation set
/// (0x does the same: cancel = mark hash unfillable). Fills are tracked in
/// taker-token units, 0x-style.
public struct FillState has store, drop {
    taker_token_filled: u64,
    cancelled: bool,
    /// Order expiry, recorded so `gc` can prove eligibility without the
    /// order bytes.
    expiry_ms: u64,
}

public struct SettlementRegistry<phantom Base, phantom Quote> has key {
    id: UID,
    fills: Table<vector<u8>, FillState>,
    /// Orders with `salt <= watermark` are void (missing entry ⇒ 0).
    salt_watermarks: Table<address, u64>,
    paused: bool,
    // -- fee config --
    /// Rate charged now; per-fill actual = `min(this, order.max_fee_bps)`.
    current_fee_bps: u64,
    /// Optional per-account override (volume discounts), admin-maintained.
    fee_tiers: Table<address, u64>,
    fee_vault_base: Balance<Base>,
    fee_vault_quote: Balance<Quote>,
    // -- market config (mirrored off-chain) --
    tick_size: u64,
    min_size: u64,
}

// === Events ===

public struct MarketCreatedEvent has copy, drop {
    registry: ID,
    base: std::string::String,
    quote: std::string::String,
    tick_size: u64,
    min_size: u64,
    fee_bps: u64,
}

public struct PauseEvent has copy, drop {
    registry: ID,
    paused: bool,
}

public struct FeeConfigEvent has copy, drop {
    registry: ID,
    fee_bps: u64,
}

// === Market listing (admin) ===

public fun create_market<Base, Quote>(
    _: &AdminCap,
    tick_size: u64,
    min_size: u64,
    fee_bps: u64,
    ctx: &mut TxContext,
): ID {
    assert!(fee_bps <= MAX_FEE_BPS, EFeeTooHigh);
    assert!(tick_size > 0 && min_size > 0, EZeroTickOrSize);
    let base = order::canonical_type<Base>();
    let quote = order::canonical_type<Quote>();
    assert!(base != quote, ESameToken);
    let reg = SettlementRegistry<Base, Quote> {
        id: object::new(ctx),
        fills: table::new(ctx),
        salt_watermarks: table::new(ctx),
        paused: false,
        current_fee_bps: fee_bps,
        fee_tiers: table::new(ctx),
        fee_vault_base: balance::zero(),
        fee_vault_quote: balance::zero(),
        tick_size,
        min_size,
    };
    let id = object::id(&reg);
    event::emit(MarketCreatedEvent { registry: id, base, quote, tick_size, min_size, fee_bps });
    transfer::share_object(reg);
    id
}

// === Admin config ===

/// Pause affects new fills only, never withdrawals — users can always exit
/// escrow (withdraw lives on BalanceManager and never touches the registry).
public fun set_paused<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    paused: bool,
) {
    reg.paused = paused;
    event::emit(PauseEvent { registry: object::id(reg), paused });
}

/// Fee cuts need no maker re-signing; hikes can never exceed what each order
/// signed (`max_fee_bps`), and never this module's hard ceiling.
public fun set_fee_bps<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    fee_bps: u64,
) {
    assert!(fee_bps <= MAX_FEE_BPS, EFeeTooHigh);
    reg.current_fee_bps = fee_bps;
    event::emit(FeeConfigEvent { registry: object::id(reg), fee_bps });
}

public fun set_fee_tier<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    account: address,
    fee_bps: u64,
) {
    assert!(fee_bps <= MAX_FEE_BPS, EFeeTooHigh);
    if (reg.fee_tiers.contains(account)) {
        *reg.fee_tiers.borrow_mut(account) = fee_bps;
    } else {
        reg.fee_tiers.add(account, fee_bps);
    }
}

public fun clear_fee_tier<Base, Quote>(
    _: &AdminCap,
    reg: &mut SettlementRegistry<Base, Quote>,
    account: address,
) {
    if (reg.fee_tiers.contains(account)) {
        reg.fee_tiers.remove(account);
    }
}

// === Storage GC (§7.8) ===

/// Permissionless cleanup: deletes fill entries for orders past
/// expiry + grace. Skips ineligible/missing digests instead of aborting so
/// batches are safe. Storage rebates flow to the caller's gas refund,
/// making cleanup self-incentivizing.
public fun gc<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    digests: vector<vector<u8>>,
    clock: &Clock,
) {
    let now = clock.timestamp_ms();
    let mut i = 0;
    while (i < digests.length()) {
        let d = digests[i];
        if (reg.fills.contains(d)) {
            let fs = reg.fills.borrow(d);
            if (fs.expiry_ms + GC_GRACE_MS <= now) {
                reg.fills.remove(d);
            }
        };
        i = i + 1;
    }
}

// === Reads ===

public fun is_paused<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): bool { reg.paused }
public fun tick_size<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): u64 { reg.tick_size }
public fun min_size<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): u64 { reg.min_size }
public fun current_fee_bps<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): u64 {
    reg.current_fee_bps
}

/// The fee rate for an account before the order's signed cap: its tier if
/// set, else the market default.
public fun fee_bps_for<Base, Quote>(
    reg: &SettlementRegistry<Base, Quote>,
    account: address,
): u64 {
    if (reg.fee_tiers.contains(account)) {
        *reg.fee_tiers.borrow(account)
    } else {
        reg.current_fee_bps
    }
}

public fun watermark<Base, Quote>(reg: &SettlementRegistry<Base, Quote>, maker: address): u64 {
    if (reg.salt_watermarks.contains(maker)) {
        *reg.salt_watermarks.borrow(maker)
    } else {
        0
    }
}

/// Cumulative taker-token filled for a digest (0 if never touched).
public fun filled<Base, Quote>(
    reg: &SettlementRegistry<Base, Quote>,
    digest: &vector<u8>,
): u64 {
    if (reg.fills.contains(*digest)) {
        reg.fills.borrow(*digest).taker_token_filled
    } else {
        0
    }
}

public fun is_cancelled<Base, Quote>(
    reg: &SettlementRegistry<Base, Quote>,
    digest: &vector<u8>,
): bool {
    if (reg.fills.contains(*digest)) {
        reg.fills.borrow(*digest).cancelled
    } else {
        false
    }
}

public fun fee_vault_base_value<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): u64 {
    reg.fee_vault_base.value()
}

public fun fee_vault_quote_value<Base, Quote>(reg: &SettlementRegistry<Base, Quote>): u64 {
    reg.fee_vault_quote.value()
}

// === Package-internal mutations (settlement / fees only) ===

/// Record `amount` of taker-token fill against a digest; creates the entry
/// on first touch. Aborts on overfill. Returns the new cumulative total.
public(package) fun record_fill<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    digest: vector<u8>,
    amount: u64,
    taker_amount_cap: u64,
    expiry_ms: u64,
): u64 {
    if (!reg.fills.contains(digest)) {
        reg.fills.add(digest, FillState { taker_token_filled: 0, cancelled: false, expiry_ms });
    };
    let fs = reg.fills.borrow_mut(digest);
    let new_total = fs.taker_token_filled + amount;
    assert!(new_total <= taker_amount_cap, EOverfill);
    fs.taker_token_filled = new_total;
    new_total
}

public(package) fun mark_cancelled<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    digest: vector<u8>,
    expiry_ms: u64,
) {
    if (!reg.fills.contains(digest)) {
        reg.fills.add(digest, FillState { taker_token_filled: 0, cancelled: true, expiry_ms });
    } else {
        reg.fills.borrow_mut(digest).cancelled = true;
    }
}

public(package) fun raise_watermark<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    maker: address,
    min_valid_salt: u64,
) {
    if (reg.salt_watermarks.contains(maker)) {
        *reg.salt_watermarks.borrow_mut(maker) = min_valid_salt;
    } else {
        reg.salt_watermarks.add(maker, min_valid_salt);
    }
}

public(package) fun deposit_fee_base<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    b: Balance<Base>,
) {
    reg.fee_vault_base.join(b);
}

public(package) fun deposit_fee_quote<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
    b: Balance<Quote>,
) {
    reg.fee_vault_quote.join(b);
}

public(package) fun take_fee_base<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
): Balance<Base> {
    reg.fee_vault_base.withdraw_all()
}

public(package) fun take_fee_quote<Base, Quote>(
    reg: &mut SettlementRegistry<Base, Quote>,
): Balance<Quote> {
    reg.fee_vault_quote.withdraw_all()
}
