//! Parametric vol smile: a strike-dependent multiplier on the ATM sigma,
//!
//! ```text
//! σ(K) = σ_atm · (1 + skew·z + convexity·z²),   z = ln(K/S) / (σ_atm·√τ)
//! ```
//!
//! `z` is the strike's distance from spot in standardized log-moneyness
//! units — the same z the scheduler's strike ladder (`grid.rs`) is built
//! on, so a wing knob maps directly onto ladder rungs. Both knobs default
//! to 0 (flat smile, today's behavior); calibrate them from observed
//! quotes before enabling. For crypto calls the upside wing usually trades
//! rich: positive `skew` raises OTM-call vol, positive `convexity` raises
//! both wings.

/// Smile parameters. `Default` is flat (multiplier 1 everywhere).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Smile {
    /// Linear coefficient per unit z.
    pub skew: f64,
    /// Quadratic coefficient per z².
    pub convexity: f64,
}

impl Smile {
    /// Sigma at strike `strike` given the ATM `sigma_atm`. Degenerate
    /// inputs (flat smile, non-positive sigma/spot/strike, expired) return
    /// `sigma_atm` unchanged. The multiplier is clamped to [0.25, 4.0] so a
    /// mis-set wing can't price vol to zero or absurdity.
    pub fn sigma_at(&self, sigma_atm: f64, spot: f64, strike: f64, t_years: f64) -> f64 {
        if (self.skew == 0.0 && self.convexity == 0.0)
            || sigma_atm <= 0.0
            || spot <= 0.0
            || strike <= 0.0
            || t_years <= 0.0
        {
            return sigma_atm;
        }
        let z = (strike / spot).ln() / (sigma_atm * t_years.sqrt());
        let mult = 1.0 + self.skew * z + self.convexity * z * z;
        sigma_atm * mult.clamp(0.25, 4.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_smile_is_identity() {
        let flat = Smile::default();
        for k in [50.0, 100.0, 150.0] {
            assert_eq!(flat.sigma_at(0.6, 100.0, k, 0.02), 0.6);
        }
    }

    #[test]
    fn atm_strike_is_unchanged_by_any_smile() {
        // z = 0 at K = S, so skew/convexity have no effect there.
        let s = Smile { skew: 0.2, convexity: 0.1 };
        assert!((s.sigma_at(0.6, 100.0, 100.0, 0.02) - 0.6).abs() < 1e-12);
    }

    #[test]
    fn positive_skew_tilts_wings() {
        let s = Smile { skew: 0.1, convexity: 0.0 };
        let up = s.sigma_at(0.6, 100.0, 130.0, 7.0 / 365.0);
        let dn = s.sigma_at(0.6, 100.0, 80.0, 7.0 / 365.0);
        assert!(up > 0.6, "upside wing {up} should be rich");
        assert!(dn < 0.6, "downside wing {dn} should be cheap");
    }

    #[test]
    fn positive_convexity_raises_both_wings() {
        let s = Smile { skew: 0.0, convexity: 0.05 };
        let up = s.sigma_at(0.6, 100.0, 130.0, 7.0 / 365.0);
        let dn = s.sigma_at(0.6, 100.0, 80.0, 7.0 / 365.0);
        assert!(up > 0.6 && dn > 0.6, "wings {up} / {dn} should both be rich");
    }

    #[test]
    fn multiplier_is_clamped() {
        // A wildly mis-set skew on a far wing must not zero out or explode vol.
        let s = Smile { skew: -10.0, convexity: 0.0 };
        let v = s.sigma_at(0.6, 100.0, 200.0, 7.0 / 365.0);
        assert!((v - 0.6 * 0.25).abs() < 1e-12, "clamped low: {v}");
        let s = Smile { skew: 10.0, convexity: 0.0 };
        let v = s.sigma_at(0.6, 100.0, 200.0, 7.0 / 365.0);
        assert!((v - 0.6 * 4.0).abs() < 1e-12, "clamped high: {v}");
    }

    #[test]
    fn degenerate_inputs_fall_through() {
        let s = Smile { skew: 0.1, convexity: 0.1 };
        assert_eq!(s.sigma_at(0.0, 100.0, 130.0, 0.02), 0.0);
        assert_eq!(s.sigma_at(0.6, 0.0, 130.0, 0.02), 0.6);
        assert_eq!(s.sigma_at(0.6, 100.0, 130.0, 0.0), 0.6);
    }
}
