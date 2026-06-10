/// Pyth wrapper (doc 03 §2): turns two `PriceInfoObject`s into the U/S
/// cross price in "settlement smallest-units per underlying
/// smallest-unit", as `(price_scaled: u128, scale: u8)` — the same shape
/// as the bucket's strike encoding, so strike/spot comparisons are exact
/// integer math.
///
/// Guardrails enforced here, so every vault crank that takes price
/// objects inherits them: feed-ID pinning (the caller cannot substitute a
/// different market), publish-time staleness, positive price, and a
/// confidence-ratio cap.
///
/// This mirrors what `mm-bot/src/pricing.rs::compute_spot_from_prices`
/// does off-chain (cross = underlying_usd / settlement_usd, rescaled by
/// the decimal difference); the two implementations are test-locked
/// against shared vectors.
module options_protocol::oracle;

use sui::clock::Clock;

use pyth::i64::I64;
use pyth::price::{Self, Price};
use pyth::price_feed;
use pyth::price_identifier;
use pyth::price_info::{Self, PriceInfoObject};

use options_protocol::errors;

/// Fixed output scale: the returned price is `price_scaled / 10^12`.
/// Comparisons against bucket strikes cross-multiply, so any consistent
/// scale works; 12 keeps sub-cent crosses (e.g. SUI/USDC at 3.5e-3) at
/// nine significant digits.
const ORACLE_PRICE_SCALE: u8 = 12;

/// Largest exponent magnitude we'll honor from a feed. Pyth publishes
/// expo ≈ −8; anything beyond ±30 is a malformed feed.
const MAX_EXPO_MAGNITUDE: u64 = 30;

/// Bound on the net rescaling exponent so the u256 intermediate cannot
/// overflow (10^38 × u64 price magnitude ≪ u256::MAX).
const MAX_NET_EXPO: u64 = 38;

/// The U/S cross with all guardrails applied.
public fun spot_cross(
    underlying_info: &PriceInfoObject,
    settlement_info: &PriceInfoObject,
    expected_underlying_feed: vector<u8>,
    expected_settlement_feed: vector<u8>,
    underlying_decimals: u8,
    settlement_decimals: u8,
    max_age_secs: u64,
    max_conf_bps: u64,
    clock: &Clock,
): (u128, u8) {
    let u_price = validated_price(
        underlying_info,
        expected_underlying_feed,
        max_age_secs,
        max_conf_bps,
        clock,
    );
    let s_price = validated_price(
        settlement_info,
        expected_settlement_feed,
        max_age_secs,
        max_conf_bps,
        clock,
    );
    cross_from_prices(&u_price, &s_price, underlying_decimals, settlement_decimals)
}

/// Extract and validate one leg: feed identity, staleness, positivity,
/// confidence ratio.
fun validated_price(
    info: &PriceInfoObject,
    expected_feed: vector<u8>,
    max_age_secs: u64,
    max_conf_bps: u64,
    clock: &Clock,
): Price {
    let price_info = price_info::get_price_info_from_price_info_object(info);
    let identifier = price_info::get_price_identifier(&price_info);
    assert!(
        price_identifier::get_bytes(&identifier) == expected_feed,
        errors::oracle_feed_mismatch(),
    );
    let p = price_feed::get_price(price_info::get_price_feed(&price_info));
    validate_price_fields(&p, max_age_secs, max_conf_bps, clock.timestamp_ms());
    p
}

/// The field checks, separated from object plumbing so tests can drive
/// them through `pyth::price::new` (a `PriceInfoObject` cannot be forged
/// outside the pyth package).
fun validate_price_fields(p: &Price, max_age_secs: u64, max_conf_bps: u64, now_ms: u64) {
    // Staleness. Publish times are seconds; a publish time slightly in
    // the future (clock skew) is fine.
    let now_secs = now_ms / 1000;
    let publish_time = price::get_timestamp(p);
    if (publish_time < now_secs) {
        assert!(now_secs - publish_time <= max_age_secs, errors::oracle_price_stale());
    };

    // Positive price.
    let price_i64 = price::get_price(p);
    assert!(!price_i64.get_is_negative(), errors::oracle_price_invalid());
    let magnitude = price_i64.get_magnitude_if_positive();
    assert!(magnitude > 0, errors::oracle_price_invalid());

    // Confidence ratio: conf / price ≤ max_conf_bps / 10⁴.
    let conf = price::get_conf(p);
    assert!(
        (conf as u128) * 10_000 <= (magnitude as u128) * (max_conf_bps as u128),
        errors::oracle_confidence(),
    );
}

/// cross = (u_mag × 10^u_expo) / (s_mag × 10^s_expo) ×
///         10^(settlement_decimals − underlying_decimals),
/// emitted at `ORACLE_PRICE_SCALE` with floor division.
fun cross_from_prices(
    u_price: &Price,
    s_price: &Price,
    underlying_decimals: u8,
    settlement_decimals: u8,
): (u128, u8) {
    let u_mag = price::get_price(u_price).get_magnitude_if_positive();
    let s_mag = price::get_price(s_price).get_magnitude_if_positive();

    // net = SCALE + u_expo − s_expo + (settle_dec − under_dec), kept as
    // positive/negative accumulators and applied to the numerator (net ≥
    // 0) or denominator (net < 0).
    let (u_expo_mag, u_expo_neg) = expo_parts(price::get_expo(u_price));
    let (s_expo_mag, s_expo_neg) = expo_parts(price::get_expo(s_price));
    let mut net_pos = (ORACLE_PRICE_SCALE as u64) + (settlement_decimals as u64);
    let mut net_neg = underlying_decimals as u64;
    if (u_expo_neg) { net_neg = net_neg + u_expo_mag } else { net_pos = net_pos + u_expo_mag };
    if (s_expo_neg) { net_pos = net_pos + s_expo_mag } else { net_neg = net_neg + s_expo_mag };
    let (num_exp, den_exp) = if (net_pos >= net_neg) {
        (net_pos - net_neg, 0u64)
    } else {
        (0u64, net_neg - net_pos)
    };
    assert!(num_exp <= MAX_NET_EXPO && den_exp <= MAX_NET_EXPO, errors::oracle_price_invalid());

    let numerator = (u_mag as u256) * pow10_u256(num_exp);
    let denominator = (s_mag as u256) * pow10_u256(den_exp);
    let cross = numerator / denominator;
    assert!(cross > 0 && cross <= (std::u128::max_value!() as u256), errors::oracle_price_invalid());
    ((cross as u128), ORACLE_PRICE_SCALE)
}

/// (magnitude, is_negative) of a Pyth I64 exponent, bounded.
fun expo_parts(expo: I64): (u64, bool) {
    if (expo.get_is_negative()) {
        let mag = expo.get_magnitude_if_negative();
        assert!(mag <= MAX_EXPO_MAGNITUDE, errors::oracle_price_invalid());
        (mag, true)
    } else {
        let mag = expo.get_magnitude_if_positive();
        assert!(mag <= MAX_EXPO_MAGNITUDE, errors::oracle_price_invalid());
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

public fun oracle_price_scale(): u8 { ORACLE_PRICE_SCALE }

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
    u_price: &Price,
    s_price: &Price,
    underlying_decimals: u8,
    settlement_decimals: u8,
): (u128, u8) {
    cross_from_prices(u_price, s_price, underlying_decimals, settlement_decimals)
}
