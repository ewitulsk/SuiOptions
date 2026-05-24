//! Black-Scholes call valuation, the MM bot's pricing model.
//!
//! `price` returns the premium in the same units as `strike` — i.e. raw
//! settlement smallest-units. Inputs:
//!
//! - `spot`         : current underlying price, settlement-asset units, scaled
//!                    by `price_scale` so we can stay in integer math.
//! - `strike`       : in the same `price_scale`d settlement-asset units.
//! - `t_years`      : time to expiry, in years (e.g., 30 days → 30/365.0).
//! - `r`            : risk-free rate, annualized continuous.
//! - `sigma`        : annualized volatility (e.g., 0.6 for 60%).
//! - `write_amount` : the option size in *underlying* smallest-units; we
//!                    multiply the per-unit BS price by this to get the
//!                    quoted premium for the whole RFQ.
//!
//! We use Abramowitz-Stegun 26.2.17 for the standard normal CDF — accurate to
//! ~7e-8, plenty for a test bot.

use tracing::trace;

#[derive(Clone, Copy, Debug)]
pub struct CallInputs {
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    pub r: f64,
    pub sigma: f64,
}

/// Per-unit-of-underlying call price in the same units as `strike`/`spot`.
pub fn call_price_per_unit(i: CallInputs) -> f64 {
    trace!(spot = i.spot, strike = i.strike, t_years = i.t_years, r = i.r, sigma = i.sigma, "computing call price");
    if i.t_years <= 0.0 {
        return (i.spot - i.strike).max(0.0);
    }
    if i.sigma <= 0.0 {
        // Deterministic forward; discounted intrinsic.
        let fwd = i.spot * (i.r * i.t_years).exp();
        let intrinsic = (fwd - i.strike).max(0.0);
        return intrinsic * (-i.r * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    let price = i.spot * norm_cdf(d1) - i.strike * (-i.r * i.t_years).exp() * norm_cdf(d2);
    trace!(price, d1, d2, "call price computed");
    price
}

/// Scale the per-unit price by the RFQ's `write_amount`, rounded down to a
/// u64. The MM bot uses this as the premium it quotes back.
pub fn premium_for_write(per_unit: f64, write_amount: u64) -> u64 {
    let total = per_unit * write_amount as f64;
    if total.is_nan() || total < 0.0 {
        return 0;
    }
    total.floor() as u64
}

/// Standard normal CDF via Abramowitz-Stegun 26.2.17.
fn norm_cdf(x: f64) -> f64 {
    // erf approximation; works on the [-∞, ∞] range with reflection.
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x_abs = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + p * x_abs);
    let y = 1.0
        - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x_abs * x_abs).exp();
    0.5 * (1.0 + sign * y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    #[test]
    fn norm_cdf_known_values() {
        // 0 → 0.5
        close(norm_cdf(0.0), 0.5, 1e-7);
        // 1.0 → 0.8413
        close(norm_cdf(1.0), 0.8413447460, 1e-6);
        // -1.0 → 0.1587
        close(norm_cdf(-1.0), 0.1586552539, 1e-6);
        // 1.96 → ~0.975
        close(norm_cdf(1.96), 0.9750021048, 1e-6);
    }

    #[test]
    fn bs_call_atm_textbook_value() {
        // Classic textbook fixture: S=K=100, T=1, r=0.05, σ=0.20 → ~10.4506
        let price = call_price_per_unit(CallInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.05,
            sigma: 0.20,
        });
        close(price, 10.4506, 0.01);
    }

    #[test]
    fn bs_call_zero_vol_collapses_to_discounted_intrinsic() {
        // σ=0 with S=110, K=100, T=1, r=0 → max(110-100, 0) = 10
        let price = call_price_per_unit(CallInputs {
            spot: 110.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.0,
            sigma: 0.0,
        });
        close(price, 10.0, 1e-9);
    }

    #[test]
    fn bs_call_zero_time_is_intrinsic() {
        let price = call_price_per_unit(CallInputs {
            spot: 105.0,
            strike: 100.0,
            t_years: 0.0,
            r: 0.05,
            sigma: 0.6,
        });
        close(price, 5.0, 1e-9);
    }

    #[test]
    fn bs_call_otm_still_positive_with_time() {
        // Far OTM but with time + vol — value should be small but > 0.
        let price = call_price_per_unit(CallInputs {
            spot: 100.0,
            strike: 150.0,
            t_years: 0.25,
            r: 0.0,
            sigma: 0.3,
        });
        assert!(price > 0.0);
        assert!(price < 5.0);
    }

    #[test]
    fn premium_scales_linearly_and_rounds_down() {
        let p = call_price_per_unit(CallInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.05,
            sigma: 0.20,
        });
        // 1 unit → ~10
        let one = premium_for_write(p, 1);
        // 100 units → ~1045
        let hundred = premium_for_write(p, 100);
        assert_eq!(one, 10);
        assert!((1040..=1050).contains(&hundred));
    }
}
