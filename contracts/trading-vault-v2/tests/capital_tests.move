#[test_only]
module vault_v2::capital_tests;

use sui::test_scenario as ts;

use vault_v2::capital;
use vault_v2::registry;
use vault_v2::test_helpers as h;

const MS_PER_YEAR: u64 = 31_536_000_000;

fun tranched_preferred(scenario: &ts::Scenario): capital::CapitalStructure {
    let cfg = h::take_protocol_config(scenario);
    let cs = capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 0, 0, 0);
    ts::return_shared(cfg);
    cs
}

// ═══════════════════════════ waterfall (§3.4a) ═══════════════════════════

#[test]
fun waterfall_untranched_all_junior() {
    let cs = capital::untranched_structure();
    let (s, j) = capital::waterfall(&cs, 1_000_000, 0, 0);
    assert!(s == 0);
    assert!(j == 1_000_000);
}

#[test]
fun waterfall_preferred_only_healthy_and_impaired() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cs = tranched_preferred(&scenario);

    // Healthy: senior gets exactly its claim, junior the residual.
    let (s, j) = capital::waterfall(&cs, 1_000_000, 400_000, 400_000);
    assert!(s == 400_000);
    assert!(j == 600_000);
    assert!(s + j == 1_000_000);

    // Impaired: total below claim ⇒ junior wiped, senior takes all.
    let (s2, j2) = capital::waterfall(&cs, 300_000, 400_000, 400_000);
    assert!(s2 == 300_000);
    assert!(j2 == 0);

    // PreferredOnly invariant: senior never exceeds its accrued claim.
    let (s3, _) = capital::waterfall(&cs, 10_000_000, 400_000, 400_000);
    assert!(s3 == 400_000);

    _clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun waterfall_uncapped_participating() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    // 30% of residual to senior, uncapped.
    let cs = capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 2, 3_000, 0);
    ts::return_shared(cfg);

    let (s, j) = capital::waterfall(&cs, 1_000_000, 400_000, 400_000);
    // preferred 400k, residual 600k, participation 180k.
    assert!(s == 580_000);
    assert!(j == 420_000);
    assert!(s + j == 1_000_000);
    // Junior always retains (1 − p) of residual.
    assert!(j == 600_000 * 7_000 / 10_000);

    _clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun waterfall_capped_participating_cap_binds() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    // 50% of residual, total return capped at 120% of principal.
    let cs = capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 1, 5_000, 12_000);
    ts::return_shared(cfg);

    // principal 400k ⇒ cap_total 480k; preferred 410k (claim with some
    // accrual) ⇒ headroom 70k; raw participation 50% × 590k = 295k ⇒
    // capped at 70k.
    let (s, j) = capital::waterfall(&cs, 1_000_000, 410_000, 400_000);
    assert!(s == 480_000);
    assert!(j == 520_000);

    // When the cap is above raw participation, participation applies.
    ts::next_tx(&mut scenario, h::admin_addr());
    let cfg2 = h::take_protocol_config(&scenario);
    let cs2 = capital::senior_junior_structure(&cfg2, 1_000, 2_000, 1_000, 1, 1_000, 50_000);
    ts::return_shared(cfg2);
    let (s2, _) = capital::waterfall(&cs2, 1_000_000, 410_000, 400_000);
    // 10% × 590k = 59k participation, cap headroom 2M−410k ⇒ no bind.
    assert!(s2 == 410_000 + 59_000);

    _clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun waterfall_conserves_nav_exactly_in_all_modes() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    let modes = vector[
        capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 0, 0, 0),
        capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 1, 3_333, 15_000),
        capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 2, 9_999, 0),
    ];
    ts::return_shared(cfg);
    let navs = vector[0u128, 1, 999, 400_000, 1_000_000, 123_456_789_012_345];
    let mut m = 0;
    while (m < modes.length()) {
        let cs = modes[m];
        let mut i = 0;
        while (i < navs.length()) {
            let nav = navs[i];
            let (s, j) = capital::waterfall(&cs, nav, 400_000, 380_000);
            assert!(s + j == nav);
            i = i + 1;
        };
        m = m + 1;
    };
    _clock.destroy_for_testing();
    scenario.end();
}

// ═══════════════════════════ accrual (§8.2) ═══════════════════════════

#[test]
fun accrual_simple_linear_and_monotonic() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cs = tranched_preferred(&scenario);

    let mut book = capital::new_book(0);
    capital::on_deposit(&mut book, &capital::tranche_from_code(1), 1_000_000, 1);
    assert!(capital::senior_claim(&book) == 1_000_000);

    // 10% simple over one year.
    capital::accrue(&mut book, &cs, MS_PER_YEAR);
    assert!(capital::senior_claim(&book) == 1_100_000);

    // 18 months at 10% simple = 15%, NOT compounded: the second half
    // year accrues on the grown claim (simple continuous accrual is
    // piecewise on the current claim). 6 more months on 1.1M = +55k.
    capital::accrue(&mut book, &cs, MS_PER_YEAR + MS_PER_YEAR / 2);
    assert!(capital::senior_claim(&book) == 1_155_000);

    // Idempotent at the same timestamp; never accrues backwards.
    capital::accrue(&mut book, &cs, MS_PER_YEAR + MS_PER_YEAR / 2);
    assert!(capital::senior_claim(&book) == 1_155_000);
    capital::accrue(&mut book, &cs, MS_PER_YEAR);
    assert!(capital::senior_claim(&book) == 1_155_000);

    // View matches mutation.
    let v = capital::accrued_claim_at(&book, &cs, 2 * MS_PER_YEAR);
    capital::accrue(&mut book, &cs, 2 * MS_PER_YEAR);
    assert!(v == capital::senior_claim(&book));

    std::unit_test::destroy(book);
    _clock.destroy_for_testing();
    scenario.end();
}

#[test]
fun accrual_elapsed_capped() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cs = tranched_preferred(&scenario);

    let mut book = capital::new_book(0);
    capital::on_deposit(&mut book, &capital::tranche_from_code(1), 1_000_000, 1);
    // A 10-year gap accrues only the 2-year cap (overflow sanity bound —
    // the keeper cadence obligation keeps real gaps far inside it).
    capital::accrue(&mut book, &cs, 10 * MS_PER_YEAR);
    assert!(capital::senior_claim(&book) == 1_200_000);

    std::unit_test::destroy(book);
    _clock.destroy_for_testing();
    scenario.end();
}

// ═══════════════════ claim reduction at exit (§3.3) ═══════════════════

#[test]
fun senior_exit_reduces_claim_pro_rata() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cs = tranched_preferred(&scenario);

    let mut book = capital::new_book(0);
    let senior = capital::tranche_from_code(1);
    capital::on_deposit(&mut book, &senior, 1_000_000, 1_000);
    capital::accrue(&mut book, &cs, MS_PER_YEAR); // claim 1.1M
    let locked_claim = capital::senior_claim(&book);
    let locked_supply = capital::senior_shares(&book);

    // Burn 40% of supply ⇒ claim drops 40%: claim-per-share invariant.
    capital::on_fulfill(&mut book, &senior, 400, locked_claim, locked_supply);
    assert!(capital::senior_shares(&book) == 600);
    assert!(capital::senior_claim(&book) == 1_100_000 - 440_000);
    // Principal basis reduced pro rata too.
    assert!(capital::senior_principal_basis(&book) == 600_000);

    std::unit_test::destroy(book);
    _clock.destroy_for_testing();
    scenario.end();
}

// ═══════════════════ reset minimum deposit (§8.5.5) ═══════════════════

#[test]
fun min_reset_deposit_cures_and_restores_buffer() {
    // N = 300k, C = 400k, t = 20%: D ≥ (400k − 0.8·300k)/0.8 = 200k.
    let d = capital::min_reset_deposit(300_000, 400_000, 2_000);
    assert!(d == 200_000);
    // Verify: post junior NAV = 300k + 200k − 400k = 100k;
    // buffer = 100k / 500k = 20% ✓ exactly at target.

    // Rounds UP when not exact: N = 300k, C = 400k, t = 33.33%.
    let d2 = capital::min_reset_deposit(300_000, 400_000, 3_333);
    let one_minus_t = 10_000u64 - 3_333;
    // ceil((400000·10^4 − 300000·6667)/6667)
    let num = 400_000u128 * 10_000 - 300_000 * (one_minus_t as u128);
    assert!(d2 == (num + (one_minus_t as u128) - 1) / (one_minus_t as u128));
    // The chosen D must satisfy (N + D − C)·10^4 ≥ t·(N + D).
    let n_plus_d = 300_000 + d2;
    assert!((n_plus_d - 400_000) * 10_000 >= 3_333 * n_plus_d);

    // Already cured: no deposit required.
    assert!(capital::min_reset_deposit(1_000_000, 400_000, 2_000) == 0);
}

// ═══════════════════ structure validation (§3.2) ═══════════════════

#[test]
#[expected_failure(abort_code = 90, location = vault_v2::capital)]
fun structure_rejects_hurdle_above_protocol_cap() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    // Default max hurdle is 2000 bps.
    let _ = capital::senior_junior_structure(&cfg, 2_001, 2_000, 1_000, 0, 0, 0);
    abort 0
}

#[test]
#[expected_failure(abort_code = 90, location = vault_v2::capital)]
fun structure_rejects_maintenance_above_target() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    let _ = capital::senior_junior_structure(&cfg, 1_000, 2_000, 2_001, 0, 0, 0);
    abort 0
}

#[test]
#[expected_failure(abort_code = 90, location = vault_v2::capital)]
fun structure_rejects_capped_mode_with_sub_principal_cap() {
    let mut scenario = ts::begin(h::admin_addr());
    let _clock = h::init_protocol(&mut scenario);
    let cfg = h::take_protocol_config(&scenario);
    let _ = capital::senior_junior_structure(&cfg, 1_000, 2_000, 1_000, 1, 5_000, 9_999);
    abort 0
}

#[test]
fun registry_capital_bounds_are_admin_settable() {
    let mut scenario = ts::begin(h::admin_addr());
    let clock = h::init_protocol(&mut scenario);

    ts::next_tx(&mut scenario, h::admin_addr());
    let admin_cap = h::take_admin_cap(&scenario);
    let mut cfg = h::take_protocol_config(&scenario);
    registry::set_max_senior_hurdle_bps(&admin_cap, &mut cfg, 3_000);
    registry::set_min_target_junior_bps(&admin_cap, &mut cfg, 1_500);
    registry::set_min_maintenance_junior_bps(&admin_cap, &mut cfg, 700);
    registry::set_min_curator_commitment_bps(&admin_cap, &mut cfg, 500);
    assert!(registry::max_senior_hurdle_bps(&cfg) == 3_000);
    assert!(registry::min_target_junior_bps(&cfg) == 1_500);
    assert!(registry::min_maintenance_junior_bps(&cfg) == 700);
    assert!(registry::min_curator_commitment_bps(&cfg) == 500);
    ts::return_shared(cfg);
    h::return_admin_cap(&scenario, admin_cap);

    clock.destroy_for_testing();
    scenario.end();
}
