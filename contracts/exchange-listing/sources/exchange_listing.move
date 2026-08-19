/// Permissionless option-market listing (SO-415).
///
/// Anyone may list an exchange market for any option series the core
/// protocol has actually created. The security argument is the `&Bucket`
/// parameter: a bucket exists only if the option currency passed
/// `option_coin`'s on-chain encoding check and the coin registry's
/// one-currency-per-type dedup, so no provenance string-parsing is needed —
/// the type system carries the proof. The quote side is structurally forced
/// to the bucket's settlement coin `S`.
///
/// Authority shape: this package parks the exchange's narrow `ListingCap`
/// inside the shared `ListingAuthority` (deposited by the deploy ceremony).
/// The exchange `AdminCap` — pause, fees, sweeps — never leaves the
/// multisig. Listing economics stay admin-controlled as per-quote defaults:
/// a quote coin without defaults is not listable, which doubles as the
/// quote allowlist.
///
/// Dedup: one market per option series, keyed by the base coin's canonical
/// type string (the quote is implied by the base's own type parameters).
/// Note the market object id is NOT precomputable off-chain — the registry
/// mints a fresh UID — so a market must exist before orders can bind its
/// id; deterministic market ids would need a derived-object claim inside
/// the exchange package (future work).
module exchange_listing::exchange_listing;

use std::string::String;
use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::event;
use sui::table::{Self, Table};

use exchange::admin::ListingCap;
use exchange::order;
use exchange::registry;
use options_core::bucket::Bucket;
use options_core::option_coin::{OptionCall, OptionPut};
use options_core::put_bucket::PutBucket;

// === Errors ===

/// No ListingCap parked in the authority (ceremony not run / cap withdrawn).
const ENoCap: u64 = 1;
/// A ListingCap is already parked; withdraw it before depositing another.
const ECapAlreadyDeposited: u64 = 2;
/// The bucket's settlement coin has no listing defaults — not an enabled quote.
const EQuoteNotEnabled: u64 = 3;
/// The option series is already expired; nothing to trade.
const EExpiredSeries: u64 = 4;
/// A market for this option series already exists.
const EAlreadyListed: u64 = 5;
/// Mirrors the registry's hard ceiling; re-asserted there at create time.
const EFeeTooHigh: u64 = 6;
const EZeroTickOrSize: u64 = 7;

const MAX_FEE_BPS: u64 = 50;

// === Objects ===

/// This package's own admin: cap custody and per-quote listing economics.
public struct AdminCap has key, store {
    id: UID,
}

/// Listing parameters for markets quoted in one settlement coin. An entry
/// existing for a coin IS the quote allowlist.
public struct MarketDefaults has copy, drop, store {
    tick_size: u64,
    min_size: u64,
    fee_bps: u64,
}

public struct ListingAuthority has key {
    id: UID,
    /// The exchange's listing delegate, deposited by the deploy ceremony.
    cap: Option<ListingCap>,
    /// Base canonical type string → SettlementRegistry id. One market per
    /// series; the quote is implied by the base's own type parameters.
    markets: Table<String, ID>,
    /// Settlement coin (original-ids TypeName) → listing economics.
    quote_defaults: Table<TypeName, MarketDefaults>,
}

// === Events ===

public struct OptionMarketListed has copy, drop {
    registry: ID,
    bucket: ID,
    base: String,
    quote: String,
    tick_size: u64,
    min_size: u64,
    fee_bps: u64,
    is_put: bool,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(ListingAuthority {
        id: object::new(ctx),
        cap: option::none(),
        markets: table::new(ctx),
        quote_defaults: table::new(ctx),
    });
    transfer::transfer(AdminCap { id: object::new(ctx) }, ctx.sender());
}

// === Cap custody (listing admin) ===

public fun deposit_cap(_: &AdminCap, auth: &mut ListingAuthority, cap: ListingCap) {
    assert!(auth.cap.is_none(), ECapAlreadyDeposited);
    auth.cap.fill(cap);
}

/// Escape hatch — deploys republish rather than upgrade, so a cap stranded
/// in a defunct authority object would otherwise be lost forever.
public fun withdraw_cap(_: &AdminCap, auth: &mut ListingAuthority): ListingCap {
    assert!(auth.cap.is_some(), ENoCap);
    auth.cap.extract()
}

// === Quote defaults (listing admin) ===

public fun set_quote_defaults<S>(
    _: &AdminCap,
    auth: &mut ListingAuthority,
    tick_size: u64,
    min_size: u64,
    fee_bps: u64,
) {
    assert!(fee_bps <= MAX_FEE_BPS, EFeeTooHigh);
    assert!(tick_size > 0 && min_size > 0, EZeroTickOrSize);
    let key = type_name::with_original_ids<S>();
    let defaults = MarketDefaults { tick_size, min_size, fee_bps };
    if (auth.quote_defaults.contains(key)) {
        *auth.quote_defaults.borrow_mut(key) = defaults;
    } else {
        auth.quote_defaults.add(key, defaults);
    }
}

public fun clear_quote_defaults<S>(_: &AdminCap, auth: &mut ListingAuthority) {
    let key = type_name::with_original_ids<S>();
    if (auth.quote_defaults.contains(key)) {
        auth.quote_defaults.remove(key);
    }
}

// === Permissionless listing ===

public fun create_call_market<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(
    auth: &mut ListingAuthority,
    bucket: &Bucket<U, S, OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>>,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(bucket.expiry_ms() > clock.timestamp_ms(), EExpiredSeries);
    let base = order::canonical_type<OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>>();
    let defaults = defaults_for<S>(auth);
    let registry_id = list<OptionCall<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>, S>(
        auth, base, defaults, ctx,
    );
    event::emit(OptionMarketListed {
        registry: registry_id,
        bucket: object::id(bucket),
        base,
        quote: order::canonical_type<S>(),
        tick_size: defaults.tick_size,
        min_size: defaults.min_size,
        fee_bps: defaults.fee_bps,
        is_put: false,
    });
    registry_id
}

public fun create_put_market<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>(
    auth: &mut ListingAuthority,
    bucket: &PutBucket<U, S, OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>>,
    clock: &Clock,
    ctx: &mut TxContext,
): ID {
    assert!(bucket.expiry_ms() > clock.timestamp_ms(), EExpiredSeries);
    let base = order::canonical_type<OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>>();
    let defaults = defaults_for<S>(auth);
    let registry_id = list<OptionPut<U, S, D0, D1, D2, D3, D4, D5, D6, D7, D8, D9>, S>(
        auth, base, defaults, ctx,
    );
    event::emit(OptionMarketListed {
        registry: registry_id,
        bucket: object::id(bucket),
        base,
        quote: order::canonical_type<S>(),
        tick_size: defaults.tick_size,
        min_size: defaults.min_size,
        fee_bps: defaults.fee_bps,
        is_put: true,
    });
    registry_id
}

fun defaults_for<S>(auth: &ListingAuthority): MarketDefaults {
    let key = type_name::with_original_ids<S>();
    assert!(auth.quote_defaults.contains(key), EQuoteNotEnabled);
    *auth.quote_defaults.borrow(key)
}

fun list<Base, Quote>(
    auth: &mut ListingAuthority,
    base: String,
    defaults: MarketDefaults,
    ctx: &mut TxContext,
): ID {
    assert!(auth.cap.is_some(), ENoCap);
    assert!(!auth.markets.contains(base), EAlreadyListed);
    let registry_id = registry::create_market_listed<Base, Quote>(
        auth.cap.borrow(),
        defaults.tick_size,
        defaults.min_size,
        defaults.fee_bps,
        ctx,
    );
    auth.markets.add(base, registry_id);
    registry_id
}

// === Reads ===

public fun has_cap(auth: &ListingAuthority): bool { auth.cap.is_some() }

public fun is_listed(auth: &ListingAuthority, base: String): bool {
    auth.markets.contains(base)
}

public fun market_for(auth: &ListingAuthority, base: String): Option<ID> {
    if (auth.markets.contains(base)) {
        option::some(*auth.markets.borrow(base))
    } else {
        option::none()
    }
}

public fun quote_enabled<S>(auth: &ListingAuthority): bool {
    auth.quote_defaults.contains(type_name::with_original_ids<S>())
}

// === Test helpers ===

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}
