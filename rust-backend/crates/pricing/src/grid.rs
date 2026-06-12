//! Z-ladder strike-grid math, shared by the option-scheduler's grid v2,
//! the vault keeper's strike selection, and the vault-sim backtester
//! (docs/vault-implementation-guide/05-offchain-services.md §4.2). One
//! implementation so the sim sees exactly the strikes production creates.

/// Per-pair z-ladders: ATM is always present and z = 1.30 ≈ the vault's
/// 0.1-delta target is always on-grid.
pub const SUI_LADDER: [f64; 5] = [0.0, 0.65, 1.30, 1.95, 2.60];
pub const BTC_LADDER: [f64; 7] = [-0.65, 0.0, 0.65, 1.30, 1.95, 2.60, 3.25];

/// Round to a "nice" exchange-style increment: the largest of
/// 10^(⌊log10 x⌋ − 2) × {1, 2.5, 5} (or a power-of-ten multiple) that
/// stays below 1% of `x`, then snap to the nearest multiple.
pub fn round_nice(x: f64) -> f64 {
    assert!(x > 0.0 && x.is_finite());
    let base = 10f64.powi(x.log10().floor() as i32 - 2);
    let mut increment = base;
    for mult in [1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0] {
        let candidate = base * mult;
        if candidate <= 0.01 * x {
            increment = candidate;
        } else {
            break;
        }
    }
    (x / increment).round() * increment
}

/// Strikes at K_i = round_nice(S · exp(z_i · σ · √τ)), strictly increasing
/// after rounding (colliding strikes bump up). Spacing adapts to the vol
/// regime automatically: ~5% intervals in calm markets, ~12% in wild ones.
pub fn build_z_ladder(spot: f64, sigma: f64, tau_years: f64, ladder: &[f64]) -> Vec<f64> {
    assert!(spot > 0.0 && sigma > 0.0 && tau_years > 0.0);
    let sqrt_tau = tau_years.sqrt();
    let mut strikes: Vec<f64> = ladder
        .iter()
        .map(|z| round_nice(spot * (z * sigma * sqrt_tau).exp()))
        .collect();
    for i in 1..strikes.len() {
        if strikes[i] <= strikes[i - 1] {
            strikes[i] = round_nice(strikes[i - 1] * 1.01);
            // round_nice can round back down onto the collision; force
            // progress with the raw bump in that case.
            if strikes[i] <= strikes[i - 1] {
                strikes[i] = strikes[i - 1] * 1.01;
            }
        }
    }
    strikes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_nice_produces_exchange_style_strikes() {
        // 117_483.91: base 10³, largest increment ≤ 1% (1174.8) is 1000.
        assert_eq!(round_nice(117_483.91), 117_000.0);
        // 3.61 at 1% = 0.0361 → increment 0.025.
        assert!((round_nice(3.61) - 3.6).abs() < 1e-12);
        // Stays within half an increment ≤ 0.5% of x.
        for x in [0.17, 3.47, 92.3, 1_234.5, 117_483.91] {
            let r = round_nice(x);
            assert!((r - x).abs() / x <= 0.005 + 1e-12, "{x} → {r}");
        }
    }

    #[test]
    fn ladder_is_strictly_increasing_and_hits_z_targets() {
        let (spot, sigma, tau) = (3.47, 0.85, 7.0 / 365.0);
        let grid = build_z_ladder(spot, sigma, tau, &SUI_LADDER);
        assert_eq!(grid.len(), 5);
        for w in grid.windows(2) {
            assert!(w[1] > w[0], "not increasing: {grid:?}");
        }
        for (k, z) in grid.iter().zip(SUI_LADDER) {
            let target = spot * (z * sigma * tau.sqrt()).exp();
            assert!((k - target).abs() / target <= 0.005 + 1e-12, "z={z}: {k} vs {target}");
        }
    }

    #[test]
    fn ladder_spacing_adapts_to_vol_regime() {
        let tau = 7.0 / 365.0;
        let calm = build_z_ladder(100.0, 0.30, tau, &SUI_LADDER);
        let wild = build_z_ladder(100.0, 1.20, tau, &SUI_LADDER);
        let calm_width = calm.last().unwrap() / calm.first().unwrap() - 1.0;
        let wild_width = wild.last().unwrap() / wild.first().unwrap() - 1.0;
        assert!(wild_width > calm_width * 3.0, "calm {calm_width}, wild {wild_width}");
    }

    #[test]
    fn collision_bumps_keep_strikes_increasing() {
        // Tiny sigma + short tenor: unrounded strikes nearly coincide.
        let grid = build_z_ladder(100.0, 0.01, 1.0 / 365.0, &SUI_LADDER);
        for w in grid.windows(2) {
            assert!(w[1] > w[0], "not increasing: {grid:?}");
        }
    }

    #[test]
    fn btc_ladder_has_itm_and_far_otm_wings() {
        let grid = build_z_ladder(117_000.0, 0.55, 7.0 / 365.0, &BTC_LADDER);
        assert_eq!(grid.len(), 7);
        assert!(grid[0] < 117_000.0, "first strike is ITM");
        assert!(*grid.last().unwrap() > 117_000.0 * 1.2, "far wing exists");
    }
}
