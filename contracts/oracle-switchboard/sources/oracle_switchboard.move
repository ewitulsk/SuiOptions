/// The second oracle adapter (SO-335): Switchboard → `PriceAttestation`.
///
/// Structural twin of `oracle_pyth`: it prices two feeds into an
/// asset→quote cross in RAW smallest-unit terms at
/// `vault_v2::price::price_scale()`, behind the same guardrails —
/// feed-hash pinning from an admin-managed registry (the caller cannot
/// substitute a different market), publish-time staleness, and a
/// positive price. Guardrail parameters are registry state, not caller
/// arguments, so a PTB cannot loosen them; vault core additionally
/// enforces its own attestation-age backstop at consumption.
///
/// The witness (`SwitchboardOracle`) is only ever constructed here, and
/// `vault_v2::price::attest` requires it to be on the protocol's
/// `OracleRegistry` allowlist — so publishing this package does nothing
/// until governance allowlists it, and delisting kills attestations
/// instantly.
///
/// ## Why this reads `Quotes` directly rather than a `QuoteVerifier`
///
/// Switchboard offers two consumption shapes. A shared `QuoteVerifier`
/// keeps a `Table` of the newest quote per feed and rejects older ones
/// across transactions (monotonicity); reading the in-PTB `Quotes`
/// bundle directly does not.
///
/// We read `Quotes` directly, deliberately:
///
/// 1. **Same posture as the Pyth adapter.** A `PriceInfoObject` can also
///    be refreshed to an older-but-still-fresh price, so Pyth gives no
///    cross-transaction monotonicity either. `max_age_secs` here, and
///    `max_price_age_ms` in vault core, are what actually bound the
///    price — and a replayed stale quote fails both.
/// 2. **No shared-object contention.** A `&mut QuoteVerifier` would
///    serialize every appraisal in the protocol behind one object.
///
/// The residual is that a caller may pick any quote inside the freshness
/// window rather than strictly the newest. That is a bounded griefing
/// surface, not a value-extraction one, and it is the surface we already
/// accept on Pyth.
module oracle_switchboard::oracle_switchboard;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::table::{Self, Table};

use switchboard::decimal;
use switchboard::quote::{Self, Quote, Quotes};

use options_core::admin::AdminCap;
use vault_v2::price::{Self as vault_price, PriceAttestation};
use vault_v2::registry::OracleRegistry;

/// Matches `vault_v2::price::PRICE_SCALE` = 10^12; asserted at
/// attestation time.
const OUTPUT_SCALE: u8 = 12;

/// `switchboard::decimal::Decimal` is fixed at 18 places. Asserted per
/// leg rather than assumed, so an upstream change fails loudly.
const SWITCHBOARD_DECIMALS: u8 = 18;

/// Bound on the net rescaling exponent so the u256 intermediate cannot
/// overflow.
const MAX_NET_EXPO: u64 = 38;

const DEFAULT_MAX_AGE_SECS: u64 = 60;

const E_FEED_NOT_CONFIGURED: u64 = 1;
const E_FEED_MISSING_FROM_BUNDLE: u64 = 2;
const E_PRICE_STALE: u64 = 3;
const E_PRICE_INVALID: u64 = 4;
const E_CONFIG_INVALID: u64 = 5;
const E_UNEXPECTED_PRECISION: u64 = 6;

/// Witness minted only by this module's attestation path.
public struct SwitchboardOracle has drop {}

public struct FeedEntry has copy, drop, store {
    /// Switchboard feed hash (32 bytes), the `feed_id` on a `Quote`.
    feed_hash: vector<u8>,
    decimals: u8,
}

/// Admin-managed: which Switchboard feed prices which coin type, that
/// coin's decimals, and the staleness guardrail.
///
/// Deliberately has no confidence cap: a Switchboard `Quote` carries no
/// confidence interval (unlike a Pyth `Price`), so there is nothing to
/// bound. Oracle-count and deviation are enforced upstream, when the
/// bundle is assembled from signed oracle responses.
public struct SwitchboardFeedRegistry has key {
    id: UID,
    feeds: Table<TypeName, FeedEntry>,
    max_age_secs: u64,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(SwitchboardFeedRegistry {
        id: object::new(ctx),
        feeds: table::new(ctx),
        max_age_secs: DEFAULT_MAX_AGE_SECS,
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun set_feed<T>(
    _: &AdminCap,
    reg: &mut SwitchboardFeedRegistry,
    feed_hash: vector<u8>,
    decimals: u8,
) {
    let t = type_name::with_defining_ids<T>();
    if (reg.feeds.contains(t)) {
        let entry = reg.feeds.borrow_mut(t);
        entry.feed_hash = feed_hash;
        entry.decimals = decimals;
    } else {
        reg.feeds.add(t, FeedEntry { feed_hash, decimals });
    };
}

public fun remove_feed<T>(_: &AdminCap, reg: &mut SwitchboardFeedRegistry) {
    let FeedEntry { feed_hash: _, decimals: _ } =
        reg.feeds.remove(type_name::with_defining_ids<T>());
}

public fun set_max_age_secs(_: &AdminCap, reg: &mut SwitchboardFeedRegistry, secs: u64) {
    assert!(secs > 0, E_CONFIG_INVALID);
    reg.max_age_secs = secs;
}

// ═══════════════════════════ attestation ═══════════════════════════

/// Price `Asset` in `Quote` raw smallest-units from the two pinned
/// Switchboard feeds, read out of one in-PTB `Quotes` bundle. The
/// attestation's timestamp is the OLDER of the two quote times, so the
/// core freshness backstop sees the weakest leg.
public fun attest<Asset, QuoteAsset>(
    feed_reg: &SwitchboardFeedRegistry,
    oracle_reg: &OracleRegistry,
    quotes: &Quotes,
    clock: &Clock,
): PriceAttestation {
    let asset_entry = feed_entry<Asset>(feed_reg);
    let quote_entry = feed_entry<QuoteAsset>(feed_reg);

    let a_quote = validated_quote(quotes, asset_entry.feed_hash, feed_reg, clock);
    let q_quote = validated_quote(quotes, quote_entry.feed_hash, feed_reg, clock);
    let cross = cross_from_quotes(
        &a_quote,
        &q_quote,
        asset_entry.decimals,
        quote_entry.decimals,
    );

    let a_ts = quote::timestamp_ms(&a_quote);
    let q_ts = quote::timestamp_ms(&q_quote);
    let ts_ms = if (a_ts < q_ts) { a_ts } else { q_ts };

    vault_price::attest(
        SwitchboardOracle {},
        oracle_reg,
        type_name::with_defining_ids<Asset>(),
        type_name::with_defining_ids<QuoteAsset>(),
        cross,
        ts_ms,
    )
}

fun feed_entry<T>(reg: &SwitchboardFeedRegistry): FeedEntry {
    let t = type_name::with_defining_ids<T>();
    assert!(reg.feeds.contains(t), E_FEED_NOT_CONFIGURED);
    *reg.feeds.borrow(t)
}

/// Pull one pinned feed out of the bundle and validate it: presence,
/// staleness, precision, sign, positivity.
fun validated_quote(
    quotes: &Quotes,
    expected_feed: vector<u8>,
    reg: &SwitchboardFeedRegistry,
    clock: &Clock,
): Quote {
    // Feed pinning: the bundle is caller-supplied, so the entry we read
    // must be the one the registry names — never "whatever is first".
    let mut maybe = quote::get_as_option(quotes, expected_feed);
    assert!(maybe.is_some(), E_FEED_MISSING_FROM_BUNDLE);
    let q = maybe.extract();
    validate_quote_fields(&q, reg.max_age_secs, clock.timestamp_ms());
    q
}

fun validate_quote_fields(q: &Quote, max_age_secs: u64, now_ms: u64) {
    let result = quote::result(q);
    validate_fields(
        quote::timestamp_ms(q),
        decimal::value(&result),
        decimal::neg(&result),
        decimal::dec(&result),
        max_age_secs,
        now_ms,
    );
}

/// The validation predicate, over plain scalars.
///
/// Split out from [`validate_quote_fields`] because `switchboard::quote`
/// exposes no constructor outside its own package — a `Quote` cannot be
/// fabricated in a test. Keeping the rules here makes them directly
/// exercisable; the wrapper above is pure field extraction.
fun validate_fields(
    ts_ms: u64,
    value: u128,
    neg: bool,
    dec: u8,
    max_age_secs: u64,
    now_ms: u64,
) {
    // A future-dated quote is not treated as fresh-by-default: only the
    // backward direction is slack, mirroring the Pyth adapter.
    if (ts_ms < now_ms) {
        assert!(now_ms - ts_ms <= max_age_secs * 1000, E_PRICE_STALE);
    };
    assert!(dec == SWITCHBOARD_DECIMALS, E_UNEXPECTED_PRECISION);
    assert!(!neg, E_PRICE_INVALID);
    assert!(value > 0, E_PRICE_INVALID);
}

/// cross = (a_val / 10^18) / (q_val / 10^18) ×
///         10^(OUTPUT_SCALE + quote_decimals − asset_decimals), with
/// floor division. Both legs share the same fixed precision, so the
/// 10^18 factors cancel and only the coin-decimal difference remains —
/// the Pyth twin additionally has to fold in each feed's own exponent.
fun cross_from_quotes(
    a_quote: &Quote,
    q_quote: &Quote,
    asset_decimals: u8,
    quote_decimals: u8,
): u128 {
    cross_from_values(
        decimal::value(&quote::result(a_quote)),
        decimal::value(&quote::result(q_quote)),
        asset_decimals,
        quote_decimals,
    )
}

/// The cross itself, over plain scalars — split out for the same reason
/// as [`validate_fields`]: `Quote` is unconstructible outside its own
/// package, so the math has to be reachable without one.
fun cross_from_values(
    a_val: u128,
    q_val: u128,
    asset_decimals: u8,
    quote_decimals: u8,
): u128 {
    let net_pos = (OUTPUT_SCALE as u64) + (quote_decimals as u64);
    let net_neg = asset_decimals as u64;
    let (num_exp, den_exp) = if (net_pos >= net_neg) {
        (net_pos - net_neg, 0u64)
    } else {
        (0u64, net_neg - net_pos)
    };
    assert!(num_exp <= MAX_NET_EXPO && den_exp <= MAX_NET_EXPO, E_PRICE_INVALID);

    let numerator = (a_val as u256) * pow10_u256(num_exp);
    let denominator = (q_val as u256) * pow10_u256(den_exp);
    let cross = numerator / denominator;
    assert!(cross > 0 && cross <= (std::u128::max_value!() as u256), E_PRICE_INVALID);

    // 10^OUTPUT_SCALE must equal core's PRICE_SCALE.
    assert!(
        pow10_u256(OUTPUT_SCALE as u64) == (vault_price::price_scale() as u256),
        E_CONFIG_INVALID,
    );
    cross as u128
}

fun pow10_u256(exp: u64): u256 {
    let mut result: u256 = 1;
    let mut i = 0;
    while (i < exp) {
        result = result * 10;
        i = i + 1;
    };
    result
}

// ══════════════════════════════ getters ══════════════════════════════

public fun max_age_secs(reg: &SwitchboardFeedRegistry): u64 { reg.max_age_secs }

public fun has_feed<T>(reg: &SwitchboardFeedRegistry): bool {
    reg.feeds.contains(type_name::with_defining_ids<T>())
}

public fun feed_hash<T>(reg: &SwitchboardFeedRegistry): vector<u8> {
    feed_entry<T>(reg).feed_hash
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}

#[test_only]
public fun validate_fields_for_testing(
    ts_ms: u64,
    value: u128,
    neg: bool,
    dec: u8,
    max_age_secs: u64,
    now_ms: u64,
) {
    validate_fields(ts_ms, value, neg, dec, max_age_secs, now_ms)
}

#[test_only]
public fun cross_from_values_for_testing(
    a_val: u128,
    q_val: u128,
    asset_decimals: u8,
    quote_decimals: u8,
): u128 {
    cross_from_values(a_val, q_val, asset_decimals, quote_decimals)
}

#[test_only]
public fun switchboard_decimals(): u8 { SWITCHBOARD_DECIMALS }
