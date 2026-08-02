#[test_only]
module oracle_switchboard::oracle_switchboard_tests;

use oracle_switchboard::oracle_switchboard as os;

const NOW_MS: u64 = 1_750_000_000_000;

/// `switchboard::decimal::Decimal` is 18-place fixed point, so a USD
/// price of `n` is `n × 10^18`.
fun usd(whole: u128, frac_18: u128): u128 {
    whole * 1_000_000_000_000_000_000 + frac_18
}

// ── cross math ───────────────────────────────────────────────────────
//
// These are the SAME vectors as `oracle_pyth::oracle_pyth_tests` (which
// in turn shares them with mm-bot's pricing.rs). Both adapters feed one
// `PriceAttestation` consumer, so a divergence here would silently
// reprice the book on an oracle switch — the equality is the point.

#[test]
fun btc_usdc_cross_matches_the_pyth_vector() {
    // BTC $50k, USDC $1, 8/6 decimals: raw-unit ratio 500 → 500 × 10¹².
    let cross = os::cross_from_values_for_testing(usd(50_000, 0), usd(1, 0), 8, 6);
    assert!(cross == 500 * 1_000_000_000_000);
}

#[test]
fun sui_usdc_sub_unit_cross_matches_the_pyth_vector() {
    // SUI $3.47, USDC $1, 9/6 decimals: 3.47e-3 → 3.47e9 at scale 12.
    let cross = os::cross_from_values_for_testing(usd(3, 470_000_000_000_000_000), usd(1, 0), 9, 6);
    assert!(cross == 3_470_000_000);
}

#[test]
fun equal_decimals_reduce_to_the_price_ratio() {
    // Same decimals on both legs ⇒ the decimal term cancels and the
    // cross is just (a/q) at scale 12.
    let cross = os::cross_from_values_for_testing(usd(4, 0), usd(2, 0), 9, 9);
    assert!(cross == 2 * 1_000_000_000_000);
}

#[test]
fun cross_floors_rather_than_rounding() {
    // 1/3 at scale 12 truncates, matching the Pyth adapter's floor
    // division — the vault's conservative-marks policy.
    let cross = os::cross_from_values_for_testing(usd(1, 0), usd(3, 0), 6, 6);
    assert!(cross == 333_333_333_333);
}

#[test]
#[expected_failure(abort_code = 4)]
fun zero_cross_is_rejected() {
    // A price so small it floors to zero must abort, not silently mark
    // the asset worthless.
    os::cross_from_values_for_testing(1, usd(1_000_000_000, 0), 0, 0);
}

// ── field validation ─────────────────────────────────────────────────

#[test]
fun quote_inside_the_freshness_window_passes() {
    os::validate_fields_for_testing(
        NOW_MS - 59_000,
        usd(1, 0),
        false,
        os::switchboard_decimals(),
        60,
        NOW_MS,
    );
}

#[test]
#[expected_failure(abort_code = 3)]
fun stale_quote_is_rejected() {
    os::validate_fields_for_testing(
        NOW_MS - 61_000,
        usd(1, 0),
        false,
        os::switchboard_decimals(),
        60,
        NOW_MS,
    );
}

#[test]
fun future_dated_quote_is_not_treated_as_stale() {
    // Clock skew forward is tolerated (the age check only runs
    // backwards); core's own backstop still bounds it at consumption.
    os::validate_fields_for_testing(
        NOW_MS + 5_000,
        usd(1, 0),
        false,
        os::switchboard_decimals(),
        60,
        NOW_MS,
    );
}

#[test]
#[expected_failure(abort_code = 4)]
fun negative_price_is_rejected() {
    os::validate_fields_for_testing(
        NOW_MS,
        usd(1, 0),
        true,
        os::switchboard_decimals(),
        60,
        NOW_MS,
    );
}

#[test]
#[expected_failure(abort_code = 4)]
fun zero_price_is_rejected() {
    os::validate_fields_for_testing(NOW_MS, 0, false, os::switchboard_decimals(), 60, NOW_MS);
}

#[test]
#[expected_failure(abort_code = 6)]
fun unexpected_precision_is_rejected() {
    // Guards the assumption that Switchboard's Decimal stays 18-place:
    // if upstream changes it, every cross would be off by orders of
    // magnitude, so fail loudly instead.
    os::validate_fields_for_testing(NOW_MS, usd(1, 0), false, 9, 60, NOW_MS);
}
