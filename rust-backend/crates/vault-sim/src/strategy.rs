//! Strike selection: the shared z-ladder grid (`pricing::grid`, doc 05
//! §4.2) plus the keeper's delta-target snap-up (doc 04 §3), combined so
//! the sim sees exactly the strikes production would create.

use pricing::strike_for_delta;

pub use pricing::grid::{build_z_ladder, round_nice, BTC_LADDER, SUI_LADDER};

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

    // round_nice / build_z_ladder behavior is tested where it lives
    // (pricing::grid); these cover the selector layered on top.

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
