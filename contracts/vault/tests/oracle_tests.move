#[test_only]
module options_vault::oracle_tests;

use pyth::i64;
use pyth::price;

use options_vault::oracle;

const NOW_MS: u64 = 1_750_000_000_000;

fun pyth_price(magnitude: u64, expo_mag: u64, expo_neg: bool, conf: u64, ts_secs: u64): price::Price {
    price::new(
        i64::new(magnitude, false),
        conf,
        i64::new(expo_mag, expo_neg),
        ts_secs,
    )
}

// --- cross math (vectors shared with mm-bot pricing.rs / spot.rs) ---

#[test]
fun test_btc_usdc_cross_matches_mm_bot_vector() {
    // BTC $50k (expo −8), USDC $1 (expo −8), TBTC(8)/TUSDC(6):
    // scale-0 cross = 500 (mm-bot's compute_spot_from_prices vector),
    // so 500 × 10¹² at the oracle's fixed scale.
    let u = pyth_price(50_000 * 100_000_000, 8, true, 10_000_000, NOW_MS / 1000);
    let s = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000);
    let (scaled, scale) = oracle::cross_from_prices_for_testing(&u, &s, 8, 6);
    assert!(scale == oracle::oracle_price_scale(), 0);
    assert!(scaled == 500 * 1_000_000_000_000, 0);
}

#[test]
fun test_sui_usdc_sub_unit_cross() {
    // SUI $3.47, USDC $1, SUI(9)/USDC(6): real cross 3.47e-3 →
    // 3.47e9 at scale 12.
    let u = pyth_price(347_000_000, 8, true, 100_000, NOW_MS / 1000);
    let s = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000);
    let (scaled, _) = oracle::cross_from_prices_for_testing(&u, &s, 9, 6);
    assert!(scaled == 3_470_000_000, 0);
}

#[test]
fun test_sub_dollar_15_cents_same_decimals() {
    // SO-55 shape: sub-dollar underlying $0.15, USDC $1, both 6 decimals:
    // cross 0.15 → 0.15e12.
    let u = pyth_price(15_000_000, 8, true, 1_000, NOW_MS / 1000);
    let s = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000);
    let (scaled, _) = oracle::cross_from_prices_for_testing(&u, &s, 6, 6);
    assert!(scaled == 150_000_000_000, 0);
}

#[test]
fun test_mixed_exponents_cross() {
    // Same $50k/$1 economics expressed at different expos: BTC as
    // 50_000 × 10^0, USDC as 1_000 × 10^-3.
    let u = pyth_price(50_000, 0, false, 1, NOW_MS / 1000);
    let s = pyth_price(1_000, 3, true, 0, NOW_MS / 1000);
    let (scaled, _) = oracle::cross_from_prices_for_testing(&u, &s, 8, 6);
    assert!(scaled == 500 * 1_000_000_000_000, 0);
}

#[test]
#[expected_failure(abort_code = 52, location = options_vault::oracle)] // oracle_price_invalid
fun test_degenerate_cross_rounding_to_zero_aborts() {
    // Underlying microscopically small vs settlement: floor(cross) == 0
    // must abort rather than hand the vault a zero spot.
    let u = pyth_price(1, 30, true, 0, NOW_MS / 1000);
    let s = pyth_price(100_000_000, 8, true, 0, NOW_MS / 1000);
    let (_, _) = oracle::cross_from_prices_for_testing(&u, &s, 9, 6);
}

// --- field validation ---

#[test]
fun test_fresh_price_within_conf_passes() {
    // 30s old, conf 10 bps, cap 100 bps.
    let p = pyth_price(347_000_000, 8, true, 347_000, NOW_MS / 1000 - 30);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
fun test_future_publish_time_is_tolerated() {
    let p = pyth_price(347_000_000, 8, true, 0, NOW_MS / 1000 + 5);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 50, location = options_vault::oracle)] // oracle_price_stale
fun test_stale_price_aborts() {
    let p = pyth_price(347_000_000, 8, true, 0, NOW_MS / 1000 - 61);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 52, location = options_vault::oracle)] // oracle_price_invalid
fun test_negative_price_aborts() {
    let p = price::new(
        i64::new(347_000_000, true),
        0,
        i64::new(8, true),
        NOW_MS / 1000,
    );
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 52, location = options_vault::oracle)] // oracle_price_invalid
fun test_zero_price_aborts() {
    let p = pyth_price(0, 8, true, 0, NOW_MS / 1000);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 51, location = options_vault::oracle)] // oracle_confidence
fun test_wide_confidence_aborts() {
    // conf/price = 5% > 1% cap.
    let p = pyth_price(100_000_000, 8, true, 5_000_000, NOW_MS / 1000);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
fun test_confidence_exactly_at_cap_passes() {
    // conf/price exactly 100 bps.
    let p = pyth_price(100_000_000, 8, true, 1_000_000, NOW_MS / 1000);
    oracle::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}
