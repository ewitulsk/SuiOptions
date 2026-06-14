//! Strike-selection equivalence goldens (README §5/§12): the keeper's
//! `pick_bucket` over a z-ladder bucket family must choose the same
//! strike `vault_sim::strategy::StrikeSelector` chooses over the same
//! ladder — that behavioral identity is what made the Ribbon backtest
//! validation transferable to production.

use keeper::state::VaultConfigView;
use keeper::strike::{pick_bucket, BucketCandidate};
use sui_types::base_types::ObjectID;
use vault_sim::strategy::StrikeSelector;

const U_DEC: u8 = 9; // SUI
const S_DEC: u8 = 6; // USDC
const WEEK_MS: u64 = 7 * 86_400_000;
const NOW: u64 = 1_750_000_000_000;

fn cfg() -> VaultConfigView {
    VaultConfigView {
        min_strike_bps_over_spot: 0, // band wide open: test the snap rule alone
        max_strike_bps_over_spot: 1_000_000,
        min_expiry_lead_ms: 3 * 86_400_000,
        max_expiry_lead_ms: 9 * 86_400_000,
        max_slice_amount: u64::MAX,
        max_open_rfqs: 4,
        round_ms: WEEK_MS,
        hold_premium_in_settlement: false,
        underlying_feed_id: vec![0u8; 32],
        settlement_feed_id: vec![0u8; 32],
        underlying_decimals: 9,
        settlement_decimals: 6,
    }
}

/// Buckets at exactly the USD strikes the scheduler's z-ladder would
/// roll, encoded in chain units at the given scale.
fn family_from_ladder(spot: f64, sigma_grid: f64, tau: f64, scale: u8) -> Vec<BucketCandidate> {
    let usd = pricing::grid::build_z_ladder(spot, sigma_grid, tau, &pricing::grid::SUI_LADDER);
    let factor = 10f64.powi(scale as i32 + S_DEC as i32 - U_DEC as i32);
    usd.iter()
        .enumerate()
        .map(|(i, k)| BucketCandidate {
            bucket_id: ObjectID::from_single_byte(i as u8 + 1),
            call_type: format!("0xc::call_{i}::CALL_{i}"),
            strike_raw: (k * factor).round() as u128,
            strike_scale: scale,
            expiry_ms: NOW + WEEK_MS,
        })
        .collect()
}

#[test]
fn keeper_pick_matches_strike_selector_across_regimes() {
    let tau = WEEK_MS as f64 / (365.0 * 86_400_000.0);
    let selector = StrikeSelector {
        delta_target: 0.10,
        ladder: pricing::grid::SUI_LADDER.to_vec(),
        r: 0.0,
        vol_floor: 0.0, // clamping is the caller's job in the keeper
        vol_ceiling: f64::INFINITY,
        };

    // (spot, σ) vectors spanning calm → wild regimes, incl. the worked
    // example from the docs ($3.47, 0.85).
    let vectors = [
        (3.47, 0.30),
        (3.47, 0.55),
        (3.47, 0.85),
        (3.47, 1.20),
        (1.05, 0.85),
        (117.0, 0.45),
    ];
    for (spot, sigma) in vectors {
        let choice = selector.select(spot, sigma, sigma, tau);
        let family = family_from_ladder(spot, sigma, tau, 7);
        let pick = pick_bucket(&family, spot, sigma, NOW, &cfg(), U_DEC, S_DEC, 0.10)
            .unwrap_or_else(|| panic!("no pick for spot={spot} sigma={sigma}"));
        let rel = (pick.strike_usd - choice.strike) / choice.strike;
        assert!(
            rel.abs() < 1e-6,
            "spot={spot} sigma={sigma}: keeper {} vs selector {} (k*={})",
            pick.strike_usd,
            choice.strike,
            choice.k_star
        );
        assert_eq!(
            pick.grid_coverage_miss, !choice.on_grid,
            "coverage flag diverged at spot={spot} sigma={sigma}"
        );
    }
}
