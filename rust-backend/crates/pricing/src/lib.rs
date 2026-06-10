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

/// Analytic call delta N(d1). Edge conventions match `call_price_per_unit`:
/// at expiry (or zero vol) the option is deterministic, so delta is the
/// indicator of the (forward) intrinsic being positive.
pub fn call_delta(i: CallInputs) -> f64 {
    if i.t_years <= 0.0 {
        return if i.spot > i.strike { 1.0 } else { 0.0 };
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * (i.r * i.t_years).exp();
        return if fwd > i.strike { 1.0 } else { 0.0 };
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    norm_cdf(d1)
}

/// The strike whose call delta equals `delta`, closed form: solving
/// N(d1) = delta for K gives
///
/// ```text
/// K = S · exp( (r + σ²/2)·τ − N⁻¹(delta)·σ·√τ )
/// ```
///
/// For small deltas (e.g. 0.10) N⁻¹(delta) is negative, so the strike lands
/// above spot. Requires `delta ∈ (0, 1)`, `sigma > 0`, `t_years > 0`.
pub fn strike_for_delta(spot: f64, sigma: f64, t_years: f64, r: f64, delta: f64) -> f64 {
    let d1 = norm_cdf_inv(delta);
    spot * ((r + 0.5 * sigma * sigma) * t_years - d1 * sigma * t_years.sqrt()).exp()
}

/// Inverse standard normal CDF via Acklam's rational approximation
/// (relative error < 1.15e-9 over the open unit interval). Returns ±∞ at
/// p = 0 / 1 and NaN outside [0, 1].
pub fn norm_cdf_inv(p: f64) -> f64 {
    if p.is_nan() || !(0.0..=1.0).contains(&p) {
        return f64::NAN;
    }
    if p == 0.0 {
        return f64::NEG_INFINITY;
    }
    if p == 1.0 {
        return f64::INFINITY;
    }

    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.38357751867269e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];
    const P_LOW: f64 = 0.02425;

    if p < P_LOW {
        // Lower tail.
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - P_LOW {
        // Central region.
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        // Upper tail: reflect the lower-tail branch.
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
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
    fn norm_cdf_inv_known_quantiles() {
        // Standard quantiles, reference values to 9 decimals.
        close(norm_cdf_inv(0.5), 0.0, 1e-9);
        close(norm_cdf_inv(0.975), 1.959963985, 1e-8);
        close(norm_cdf_inv(0.025), -1.959963985, 1e-8);
        // The vault's 0.10-delta constant (doc 04 §3: z* = −N⁻¹(0.10)).
        close(norm_cdf_inv(0.10), -1.281551566, 1e-8);
        close(norm_cdf_inv(0.8413447460), 1.0, 1e-8);
        // Tail branches.
        close(norm_cdf_inv(0.001), -3.090232306, 1e-8);
        close(norm_cdf_inv(0.999), 3.090232306, 1e-8);
    }

    #[test]
    fn norm_cdf_inv_round_trips_norm_cdf() {
        // norm_cdf is A-S 26.2.17 (~7e-8 abs error), so the round trip is
        // bounded by its accuracy, not Acklam's.
        let mut x = -6.0;
        while x <= 6.0 {
            let p = norm_cdf(x);
            if p > 0.0 && p < 1.0 {
                let x_dx_dp = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
                // Compare in probability space to avoid tail blow-up of dp→dx.
                close(norm_cdf(norm_cdf_inv(p)), p, 1e-7 + 1e-9 / x_dx_dp.max(1e-12));
            }
            x += 0.01;
        }
    }

    #[test]
    fn norm_cdf_inv_edges() {
        assert_eq!(norm_cdf_inv(0.0), f64::NEG_INFINITY);
        assert_eq!(norm_cdf_inv(1.0), f64::INFINITY);
        assert!(norm_cdf_inv(-0.1).is_nan());
        assert!(norm_cdf_inv(1.1).is_nan());
        assert!(norm_cdf_inv(f64::NAN).is_nan());
    }

    #[test]
    fn call_delta_matches_bump_and_reprice() {
        // Analytic delta vs central finite difference. The agreement floor
        // is set by the A-S 26.2.17 norm_cdf (~1.5e-7 abs error): its error
        // term's derivative breaks the exact S·φ(d1) = K·e^{−rτ}·φ(d2)
        // cancellation by up to ~1e-5, so we test to 2e-5 rather than the
        // 1e-6 an exact CDF would allow.
        let cases = [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 130.0, 7.0 / 365.0, 0.0, 0.60),
            (3.5, 4.2, 14.0 / 365.0, 0.03, 0.90),
            (77_000.0, 90_000.0, 30.0 / 365.0, 0.04, 0.45),
        ];
        for (spot, strike, t_years, r, sigma) in cases {
            let i = CallInputs { spot, strike, t_years, r, sigma };
            let h = spot * 1e-5;
            let up = call_price_per_unit(CallInputs { spot: spot + h, ..i });
            let dn = call_price_per_unit(CallInputs { spot: spot - h, ..i });
            let numeric = (up - dn) / (2.0 * h);
            close(call_delta(i), numeric, 2e-5);
        }
    }

    #[test]
    fn call_delta_edges() {
        // Expired: indicator of intrinsic.
        let base = CallInputs { spot: 105.0, strike: 100.0, t_years: 0.0, r: 0.0, sigma: 0.5 };
        close(call_delta(base), 1.0, 1e-12);
        close(call_delta(CallInputs { spot: 95.0, ..base }), 0.0, 1e-12);
        // Zero vol: indicator on the forward.
        let det = CallInputs { spot: 100.0, strike: 101.0, t_years: 1.0, r: 0.05, sigma: 0.0 };
        close(call_delta(det), 1.0, 1e-12); // fwd ≈ 105.13 > 101
        close(call_delta(CallInputs { strike: 110.0, ..det }), 0.0, 1e-12);
    }

    #[test]
    fn strike_for_delta_round_trips_to_target_delta() {
        // The keeper's exact use case: weekly 0.10-delta strike (doc 04 §3).
        for (delta, sigma, t_years) in [
            (0.10, 0.60, 7.0 / 365.0),
            (0.10, 1.20, 7.0 / 365.0),
            (0.05, 0.45, 7.0 / 365.0),
            (0.30, 0.80, 14.0 / 365.0),
        ] {
            let spot = 3.5;
            let k = strike_for_delta(spot, sigma, t_years, 0.0, delta);
            assert!(k > spot, "small-delta strike must be above spot");
            let realized = call_delta(CallInputs {
                spot,
                strike: k,
                t_years,
                r: 0.0,
                sigma,
            });
            close(realized, delta, 1e-7);
        }
    }

    #[test]
    fn strike_for_delta_matches_doc_formula() {
        // Doc 04 §3: K* = S·exp((r + σ²/2)·τ + z*·σ·√τ), z* = −N⁻¹(0.10) = 1.281552.
        let (spot, sigma, t, r): (f64, f64, f64, f64) = (100.0, 0.60, 7.0 / 365.0, 0.0);
        let z_star = 1.281552;
        let expected = spot * ((r + 0.5 * sigma * sigma) * t + z_star * sigma * t.sqrt()).exp();
        close(strike_for_delta(spot, sigma, t, r, 0.10), expected, 1e-3);
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
