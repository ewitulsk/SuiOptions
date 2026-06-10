//! Strike selection: the z-ladder grid (doc 05 §4.2) plus the keeper's
//! delta-target snap-up (doc 04 §3), combined so the sim sees exactly the
//! strikes production would create. The ladder constants and `round_nice`
//! here are the shared reference for the option-scheduler's grid v2 and
//! the keeper's `strike.rs`.

use pricing::strike_for_delta;

/// Per-pair z-ladders (doc 05 §4.2): ATM is always present and z = 1.30 ≈
/// the vault's 0.1-delta target is always on-grid.
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

/// Strikes at K_i = round_nice(S · exp(z_i · σ · √τ)) (doc 05 §4.2),
/// strictly increasing after rounding (colliding strikes bump one
/// increment up — here: nudged by 1% and re-rounded).
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

/// The vault's strike choice for one round.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrikeChoice {
    pub strike: f64,
    /// The unsnapped delta-target strike K*.
    pub k_star: f64,
    /// False when no grid strike ≥ K* existed and the selector fell back
    /// to the highest strike (the keeper's `GridCoverageMiss`, doc 04 §3).
    pub on_grid: bool,
}

/// Doc 04 §3: compute K* for the delta target from the pricing IV, then
/// take the smallest grid strike ≥ K* (snap up ⇒ delta ≤ target ⇒
/// conservative). The grid itself is built from the *grid* vol — realized,
/// clamped — exactly as the scheduler will build it.
#[derive(Clone, Debug)]
pub struct StrikeSelector {
    pub delta_target: f64,
    pub ladder: Vec<f64>,
    pub r: f64,
    /// Clamp applied to the grid sigma (scheduler's vol_floor/ceiling).
    pub vol_floor: f64,
    pub vol_ceiling: f64,
}

impl StrikeSelector {
    pub fn select(
        &self,
        spot: f64,
        sigma_grid: f64,
        sigma_iv: f64,
        tau_years: f64,
    ) -> StrikeChoice {
        let sigma_grid = sigma_grid.clamp(self.vol_floor, self.vol_ceiling);
        let grid = build_z_ladder(spot, sigma_grid, tau_years, &self.ladder);
        let k_star = strike_for_delta(spot, sigma_iv, tau_years, self.r, self.delta_target);
        match grid.iter().copied().find(|k| *k >= k_star) {
            Some(strike) => StrikeChoice { strike, k_star, on_grid: true },
            None => StrikeChoice {
                strike: *grid.last().expect("ladder is non-empty"),
                k_star,
                on_grid: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pricing::{call_delta, CallInputs};

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
        // Doc 07 §1.2: ladder hits z-targets within half a nice-rounding
        // step; strictly increasing.
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
        // Compare relative grid widths (top/bottom − 1): 4× the vol means
        // ~4× the width.
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
    fn selector_snaps_up_and_lands_at_or_below_target_delta() {
        let sel = StrikeSelector {
            delta_target: 0.10,
            ladder: SUI_LADDER.to_vec(),
            r: 0.0,
            vol_floor: 0.2,
            vol_ceiling: 2.0,
        };
        let (spot, sigma, tau) = (3.47, 0.85, 7.0 / 365.0);
        let choice = sel.select(spot, sigma, sigma, tau);
        assert!(choice.on_grid);
        assert!(choice.strike >= choice.k_star, "snap must go up");
        // Snapping up means realized delta ≤ target (small rounding slack
        // for the nice-rounding of the grid point itself).
        let realized = call_delta(CallInputs {
            spot,
            strike: choice.strike,
            t_years: tau,
            r: 0.0,
            sigma,
        });
        assert!(realized <= 0.10 + 0.01, "delta {realized}");
        // z = 1.30 is the designed 0.1-delta gridpoint: the choice should
        // be at or adjacent to it.
        let grid = build_z_ladder(spot, sigma, tau, &SUI_LADDER);
        assert!(choice.strike >= grid[1] && choice.strike <= grid[3], "{choice:?} in {grid:?}");
    }

    #[test]
    fn selector_falls_back_to_highest_strike_off_grid() {
        // IV far above the (clamped) grid vol pushes K* beyond the ladder.
        let sel = StrikeSelector {
            delta_target: 0.10,
            ladder: SUI_LADDER.to_vec(),
            r: 0.0,
            vol_floor: 0.2,
            vol_ceiling: 0.3, // clamp the grid low
        };
        let choice = sel.select(100.0, 3.0, 3.0, 7.0 / 365.0);
        assert!(!choice.on_grid);
        let grid = build_z_ladder(100.0, 0.3, 7.0 / 365.0, &SUI_LADDER);
        assert_eq!(choice.strike, *grid.last().unwrap());
    }
}
