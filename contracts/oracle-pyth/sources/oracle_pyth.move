/// The first oracle adapter (design doc §4.1): Pyth → `PriceAttestation`.
///
/// Prices two `PriceInfoObject`s into an asset→quote cross in RAW
/// smallest-unit terms at `trading_vault::price::price_scale()` — the
/// same math as `options_vault::oracle::spot_cross` (cross =
/// asset_usd / quote_usd, rescaled by the decimal difference), with the
/// same guardrails: feed-ID pinning from an admin-managed registry (the
/// caller cannot substitute a different market), publish-time staleness,
/// positive price, and a confidence-ratio cap. Guardrail parameters are
/// registry state, not caller arguments, so a PTB cannot loosen them;
/// vault core additionally enforces its own attestation-age backstop at
/// consumption.
///
/// The witness (`PythOracle`) is only ever constructed here, and
/// `trading_vault::price::attest` requires it to be on the protocol's
/// `OracleRegistry` allowlist — so deploying this package does nothing
/// until governance allowlists it, and delisting it kills attestations
/// instantly.
module oracle_pyth::oracle_pyth;

use std::type_name::{Self, TypeName};
use sui::clock::Clock;
use sui::table::{Self, Table};

use pyth::i64::I64;
use pyth::price::{Self, Price};
use pyth::price_feed;
use pyth::price_identifier;
use pyth::price_info::{Self, PriceInfoObject};

use options_core::admin::AdminCap;
use trading_vault::price::{Self as vault_price, PriceAttestation};
use trading_vault::registry::OracleRegistry;

/// Matches `trading_vault::price::PRICE_SCALE` = 10^12; asserted at
/// attestation time.
const OUTPUT_SCALE: u8 = 12;

/// Largest exponent magnitude honored from a feed (Pyth publishes
/// expo ≈ −8; beyond ±30 is malformed).
const MAX_EXPO_MAGNITUDE: u64 = 30;

/// Bound on the net rescaling exponent so the u256 intermediate cannot
/// overflow.
const MAX_NET_EXPO: u64 = 38;

const DEFAULT_MAX_AGE_SECS: u64 = 60;
const DEFAULT_MAX_CONF_BPS: u64 = 100;

const E_FEED_NOT_CONFIGURED: u64 = 1;
const E_FEED_MISMATCH: u64 = 2;
const E_PRICE_STALE: u64 = 3;
const E_PRICE_INVALID: u64 = 4;
const E_CONFIDENCE: u64 = 5;
const E_CONFIG_INVALID: u64 = 6;

/// Witness minted only by this module's attestation path.
public struct PythOracle has drop {}

public struct FeedEntry has copy, drop, store {
    feed_id: vector<u8>,
    decimals: u8,
}

/// Admin-managed: which Pyth feed prices which coin type, that coin's
/// decimals, and the staleness/confidence guardrails.
public struct PythFeedRegistry has key {
    id: UID,
    feeds: Table<TypeName, FeedEntry>,
    max_age_secs: u64,
    max_conf_bps: u64,
}

fun init(ctx: &mut TxContext) {
    transfer::share_object(PythFeedRegistry {
        id: object::new(ctx),
        feeds: table::new(ctx),
        max_age_secs: DEFAULT_MAX_AGE_SECS,
        max_conf_bps: DEFAULT_MAX_CONF_BPS,
    });
}

// ═══════════════════════════════ admin ═══════════════════════════════

public fun set_feed<T>(
    _: &AdminCap,
    reg: &mut PythFeedRegistry,
    feed_id: vector<u8>,
    decimals: u8,
) {
    let t = type_name::with_defining_ids<T>();
    if (reg.feeds.contains(t)) {
        let entry = reg.feeds.borrow_mut(t);
        entry.feed_id = feed_id;
        entry.decimals = decimals;
    } else {
        reg.feeds.add(t, FeedEntry { feed_id, decimals });
    };
}

public fun remove_feed<T>(_: &AdminCap, reg: &mut PythFeedRegistry) {
    let FeedEntry { feed_id: _, decimals: _ } =
        reg.feeds.remove(type_name::with_defining_ids<T>());
}

public fun set_max_age_secs(_: &AdminCap, reg: &mut PythFeedRegistry, secs: u64) {
    assert!(secs > 0, E_CONFIG_INVALID);
    reg.max_age_secs = secs;
}

public fun set_max_conf_bps(_: &AdminCap, reg: &mut PythFeedRegistry, bps: u64) {
    assert!(bps > 0, E_CONFIG_INVALID);
    reg.max_conf_bps = bps;
}

// ═══════════════════════════ attestation ═══════════════════════════

/// Price `Asset` in `Quote` raw smallest-units from the two pinned Pyth
/// feeds. The attestation's timestamp is the OLDER of the two publish
/// times, so the core freshness backstop sees the weakest leg.
public fun attest<Asset, Quote>(
    feed_reg: &PythFeedRegistry,
    oracle_reg: &OracleRegistry,
    asset_info: &PriceInfoObject,
    quote_info: &PriceInfoObject,
    clock: &Clock,
): PriceAttestation {
    let asset_entry = feed_entry<Asset>(feed_reg);
    let quote_entry = feed_entry<Quote>(feed_reg);

    let a_price = validated_price(asset_info, asset_entry.feed_id, feed_reg, clock);
    let q_price = validated_price(quote_info, quote_entry.feed_id, feed_reg, clock);
    let cross = cross_from_prices(&a_price, &q_price, asset_entry.decimals, quote_entry.decimals);

    let a_ts = price::get_timestamp(&a_price);
    let q_ts = price::get_timestamp(&q_price);
    let ts_secs = if (a_ts < q_ts) { a_ts } else { q_ts };

    vault_price::attest(
        PythOracle {},
        oracle_reg,
        type_name::with_defining_ids<Asset>(),
        type_name::with_defining_ids<Quote>(),
        cross,
        ts_secs * 1000,
    )
}

fun feed_entry<T>(reg: &PythFeedRegistry): FeedEntry {
    let t = type_name::with_defining_ids<T>();
    assert!(reg.feeds.contains(t), E_FEED_NOT_CONFIGURED);
    *reg.feeds.borrow(t)
}

/// Extract and validate one leg: feed identity, staleness, positivity,
/// confidence ratio. Mirrors `options_vault::oracle::validated_price`.
fun validated_price(
    info: &PriceInfoObject,
    expected_feed: vector<u8>,
    reg: &PythFeedRegistry,
    clock: &Clock,
): Price {
    let price_info = price_info::get_price_info_from_price_info_object(info);
    let identifier = price_info::get_price_identifier(&price_info);
    assert!(price_identifier::get_bytes(&identifier) == expected_feed, E_FEED_MISMATCH);
    let p = price_feed::get_price(price_info::get_price_feed(&price_info));
    validate_price_fields(&p, reg.max_age_secs, reg.max_conf_bps, clock.timestamp_ms());
    p
}

fun validate_price_fields(p: &Price, max_age_secs: u64, max_conf_bps: u64, now_ms: u64) {
    let now_secs = now_ms / 1000;
    let publish_time = price::get_timestamp(p);
    if (publish_time < now_secs) {
        assert!(now_secs - publish_time <= max_age_secs, E_PRICE_STALE);
    };

    let price_i64 = price::get_price(p);
    assert!(!price_i64.get_is_negative(), E_PRICE_INVALID);
    let magnitude = price_i64.get_magnitude_if_positive();
    assert!(magnitude > 0, E_PRICE_INVALID);

    let conf = price::get_conf(p);
    assert!(
        (conf as u128) * 10_000 <= (magnitude as u128) * (max_conf_bps as u128),
        E_CONFIDENCE,
    );
}

/// cross = (a_mag × 10^a_expo) / (q_mag × 10^q_expo) ×
///         10^(quote_decimals − asset_decimals), at `OUTPUT_SCALE` with
/// floor division. Mirrors `options_vault::oracle::cross_from_prices`.
fun cross_from_prices(
    a_price: &Price,
    q_price: &Price,
    asset_decimals: u8,
    quote_decimals: u8,
): u128 {
    let a_mag = price::get_price(a_price).get_magnitude_if_positive();
    let q_mag = price::get_price(q_price).get_magnitude_if_positive();

    let (a_expo_mag, a_expo_neg) = expo_parts(price::get_expo(a_price));
    let (q_expo_mag, q_expo_neg) = expo_parts(price::get_expo(q_price));
    let mut net_pos = (OUTPUT_SCALE as u64) + (quote_decimals as u64);
    let mut net_neg = asset_decimals as u64;
    if (a_expo_neg) { net_neg = net_neg + a_expo_mag } else { net_pos = net_pos + a_expo_mag };
    if (q_expo_neg) { net_pos = net_pos + q_expo_mag } else { net_neg = net_neg + q_expo_mag };
    let (num_exp, den_exp) = if (net_pos >= net_neg) {
        (net_pos - net_neg, 0u64)
    } else {
        (0u64, net_neg - net_pos)
    };
    assert!(num_exp <= MAX_NET_EXPO && den_exp <= MAX_NET_EXPO, E_PRICE_INVALID);

    let numerator = (a_mag as u256) * pow10_u256(num_exp);
    let denominator = (q_mag as u256) * pow10_u256(den_exp);
    let cross = numerator / denominator;
    assert!(cross > 0 && cross <= (std::u128::max_value!() as u256), E_PRICE_INVALID);

    // 10^OUTPUT_SCALE must equal core's PRICE_SCALE.
    assert!(pow10_u256(OUTPUT_SCALE as u64) == (vault_price::price_scale() as u256), E_CONFIG_INVALID);
    cross as u128
}

fun expo_parts(expo: I64): (u64, bool) {
    if (expo.get_is_negative()) {
        let mag = expo.get_magnitude_if_negative();
        assert!(mag <= MAX_EXPO_MAGNITUDE, E_PRICE_INVALID);
        (mag, true)
    } else {
        let mag = expo.get_magnitude_if_positive();
        assert!(mag <= MAX_EXPO_MAGNITUDE, E_PRICE_INVALID);
        (mag, false)
    }
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

public fun max_age_secs(reg: &PythFeedRegistry): u64 { reg.max_age_secs }

public fun max_conf_bps(reg: &PythFeedRegistry): u64 { reg.max_conf_bps }

public fun has_feed<T>(reg: &PythFeedRegistry): bool {
    reg.feeds.contains(type_name::with_defining_ids<T>())
}

#[test_only]
public fun init_for_testing(ctx: &mut TxContext) {
    init(ctx)
}

#[test_only]
public fun validate_price_fields_for_testing(
    p: &Price,
    max_age_secs: u64,
    max_conf_bps: u64,
    now_ms: u64,
) {
    validate_price_fields(p, max_age_secs, max_conf_bps, now_ms)
}

#[test_only]
public fun cross_from_prices_for_testing(
    a_price: &Price,
    q_price: &Price,
    asset_decimals: u8,
    quote_decimals: u8,
): u128 {
    cross_from_prices(a_price, q_price, asset_decimals, quote_decimals)
}
