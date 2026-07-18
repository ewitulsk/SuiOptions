#[test_only]
module oracle_pyth::oracle_pyth_tests;

use pyth::i64;
use pyth::price;

use oracle_pyth::oracle_pyth as op;

const NOW_MS: u64 = 1_750_000_000_000;

fun pyth_price(magnitude: u64, expo_mag: u64, expo_neg: bool, conf: u64, ts_secs: u64): price::Price {
    price::new(
        i64::new(magnitude, false),
        conf,
        i64::new(expo_mag, expo_neg),
        ts_secs,
    )
}

// Cross-math vectors shared with options_vault::oracle_tests (and
// mm-bot pricing.rs): the adapter's cross must agree with the
// covered-call oracle it was lifted from.

#[test]
fun btc_usdc_cross_matches_shared_vector() {
    // BTC $50k (expo −8), USDC $1 (expo −8), 8/6 decimals: raw-unit
    // ratio 500 → 500 × 10¹² at the fixed scale.
    let a = pyth_price(50_000 * 100_000_000, 8, true, 10_000_000, NOW_MS / 1000);
    let q = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000);
    let cross = op::cross_from_prices_for_testing(&a, &q, 8, 6);
    assert!(cross == 500 * 1_000_000_000_000);
}

#[test]
fun sui_usdc_sub_unit_cross() {
    // SUI $3.47, USDC $1, 9/6 decimals: 3.47e-3 → 3.47e9 at scale 12.
    let a = pyth_price(347_000_000, 8, true, 100_000, NOW_MS / 1000);
    let q = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000);
    let cross = op::cross_from_prices_for_testing(&a, &q, 9, 6);
    assert!(cross == 3_470_000_000);
}

#[test]
fun mixed_exponents_cross() {
    // Same $50k/$1 economics at odd expos: 50_000 × 10^0 vs 1_000 × 10^-3.
    let a = pyth_price(50_000, 0, false, 1, NOW_MS / 1000);
    let q = pyth_price(1_000, 3, true, 0, NOW_MS / 1000);
    let cross = op::cross_from_prices_for_testing(&a, &q, 8, 6);
    assert!(cross == 500 * 1_000_000_000_000);
}

#[test]
fun fresh_price_within_age_passes() {
    let p = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000 - 30);
    op::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 3, location = oracle_pyth::oracle_pyth)]
fun stale_price_rejected() {
    let p = pyth_price(100_000_000, 8, true, 10_000, NOW_MS / 1000 - 120);
    op::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 5, location = oracle_pyth::oracle_pyth)]
fun wide_confidence_rejected() {
    // conf 5% of price against a 1% cap.
    let p = pyth_price(100_000_000, 8, true, 5_000_000, NOW_MS / 1000);
    op::validate_price_fields_for_testing(&p, 60, 100, NOW_MS);
}
