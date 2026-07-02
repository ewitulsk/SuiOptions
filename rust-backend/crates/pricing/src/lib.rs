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
//! The standard normal CDF is computed from `libm::erfc`, accurate to ~1 ulp.
//!
//! Rate convention: the protocol prices with `r = 0` everywhere (keeper,
//! api-service, vault-sim, mm-bot). Settlement is a stablecoin with no funded
//! rate leg, so a nonzero r has nothing to hedge against; r = 0 also makes
//! these European formulas exact for the protocol's American-exercisable
//! options (early exercise of a call on a non-yielding asset, or of a put at
//! r = 0, is never optimal). The `r` parameter stays for tests and for any
//! future funded-rate use.

pub mod grid;

use tracing::trace;

#[derive(Clone, Copy, Debug)]
pub struct CallInputs {
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    pub r: f64,
    pub sigma: f64,
}

/// Black-Scholes inputs for a put. Identical shape to [`CallInputs`] (spot,
/// strike, time, rate, vol are option-type-agnostic); aliased for call-site
/// clarity in the put functions below.
pub type PutInputs = CallInputs;

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
/// u64. The MM bot uses this for the premium it *pays* (bid side); rounding
/// down never overpays.
pub fn premium_for_write(per_unit: f64, write_amount: u64) -> u64 {
    let total = per_unit * write_amount as f64;
    if total.is_nan() || total < 0.0 {
        return 0;
    }
    total.floor() as u64
}

/// Like [`premium_for_write`] but rounded up — for the premium the MM
/// *charges* (ask side), where rounding down would systematically undercharge
/// by up to one settlement raw-unit per quote.
pub fn premium_for_write_ceil(per_unit: f64, write_amount: u64) -> u64 {
    let total = per_unit * write_amount as f64;
    if total.is_nan() || total <= 0.0 {
        return 0;
    }
    total.ceil() as u64
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

/// Risk-neutral probability the call finishes in-the-money — i.e. the vault
/// gets assigned — which is `N(d2)`. Distinct from delta (`N(d1)`): for an OTM
/// call `N(d2) < N(d1)`, so a 0.10-delta strike has a *lower* assignment
/// probability than 0.10. Edge conventions match `call_delta`: at expiry or
/// zero vol the payoff is deterministic, so this collapses to the (forward)
/// intrinsic indicator.
pub fn assignment_prob(i: CallInputs) -> f64 {
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
    let d2 = d1 - i.sigma * sqrt_t;
    norm_cdf(d2)
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    /// ∂price/∂σ per 1.00 (=100%) vol. Divide by 100 for per-1%-vol.
    pub vega: f64,
    /// Per-CALENDAR-DAY theta (annual θ ÷ 365) to match retail screens.
    pub theta: f64,
    /// ∂price/∂r per 1.00 (=100%) rate. Divide by 100 for per-1%-rate.
    pub rho: f64,
}

/// Analytic Black-Scholes call greeks. Conventions match `call_price_per_unit`
/// / `call_delta`: at expiry or zero vol the option is deterministic, so the
/// smooth greeks vanish and delta is the (forward) intrinsic indicator.
pub fn call_greeks(i: CallInputs) -> Greeks {
    if i.t_years <= 0.0 || i.sigma <= 0.0 {
        return Greeks {
            delta: call_delta(i),
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
            rho: 0.0,
        };
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    let pdf_d1 = norm_pdf(d1);
    let disc = (-i.r * i.t_years).exp();

    let delta = norm_cdf(d1);
    let gamma = pdf_d1 / (i.spot * i.sigma * sqrt_t);
    let vega = i.spot * pdf_d1 * sqrt_t;
    // Annualized call theta, then converted to per-day below.
    let theta_annual =
        -(i.spot * pdf_d1 * i.sigma) / (2.0 * sqrt_t) - i.r * i.strike * disc * norm_cdf(d2);
    let rho = i.strike * i.t_years * disc * norm_cdf(d2);

    Greeks { delta, gamma, vega, theta: theta_annual / 365.0, rho }
}

/// Implied volatility of a call from its observed price, via Newton-Raphson
/// seeded with the Brenner-Subrahmanyam ATM approximation, with a bisection
/// fallback if Newton stalls (flat vega / overshoot). `market`, `spot`,
/// `strike` share one unit. Returns `None` when there is no positive-vol
/// solution: `market` below the no-arbitrage intrinsic, `market` ≥ `spot`, or
/// expiry ≤ 0. Result is clamped to (0, 5.0].
pub fn implied_vol(market: f64, spot: f64, strike: f64, t_years: f64, r: f64) -> Option<f64> {
    if !market.is_finite() || market <= 0.0 || t_years <= 0.0 || spot <= 0.0 {
        return None;
    }
    // No-arbitrage call bounds: max(S − K·e^(−rτ), 0) ≤ C < S.
    let intrinsic = (spot - strike * (-r * t_years).exp()).max(0.0);
    if market < intrinsic || market >= spot {
        return None;
    }

    let price_at =
        |sigma: f64| call_price_per_unit(CallInputs { spot, strike, t_years, r, sigma });
    let vega_at = |sigma: f64| {
        let sqrt_t = t_years.sqrt();
        let d1 = ((spot / strike).ln() + (r + 0.5 * sigma * sigma) * t_years) / (sigma * sqrt_t);
        spot * norm_pdf(d1) * sqrt_t
    };

    // Brenner-Subrahmanyam seed: σ₀ ≈ √(2π/τ)·(C/S). Good near ATM.
    let mut sigma =
        ((2.0 * std::f64::consts::PI / t_years).sqrt() * market / spot).clamp(1e-3, 5.0);

    for _ in 0..100 {
        let diff = price_at(sigma) - market;
        if diff.abs() < 1e-9 {
            return Some(sigma.clamp(1e-6, 5.0));
        }
        let v = vega_at(sigma);
        if !v.is_finite() || v < 1e-12 {
            break; // vega too flat — hand off to bisection
        }
        let next = sigma - diff / v;
        if !next.is_finite() {
            break;
        }
        sigma = next.clamp(1e-6, 5.0);
    }

    // Bisection on [1e-6, 5.0]; price is monotone increasing in σ.
    let (mut lo, mut hi) = (1e-6_f64, 5.0_f64);
    if price_at(lo) > market || price_at(hi) < market {
        return None; // outside the bracket we can solve
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        let p = price_at(mid);
        if (p - market).abs() < 1e-9 {
            return Some(mid);
        }
        if p < market {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(0.5 * (lo + hi))
}

/// Break-even underlying price for a long call held to expiry:
/// `strike + premium_per_unit`. The caller chooses which premium to pass —
/// the live mid for a pre-trade quote, or the average cost for an open
/// position (the latter is what produces the screenshot's 537.15 = 500 + 37.15).
pub fn break_even(strike: f64, premium_per_unit: f64) -> f64 {
    strike + premium_per_unit
}

// ---- cash-secured puts (mirror of the call functions above) ----
//
// Every put function shares the call edge conventions: at expiry the value is
// the raw intrinsic `max(K − S, 0)`; at zero vol it is the discounted forward
// intrinsic; otherwise Black-Scholes. The relationships to the call side are
// exact (put–call parity `C − P = S − K·e^(−rτ)`), which the tests pin.

/// Per-unit-of-underlying European put price, in the same units as
/// `strike`/`spot`: `K·e^(−rτ)·N(−d2) − S·N(−d1)`.
pub fn put_price_per_unit(i: PutInputs) -> f64 {
    trace!(spot = i.spot, strike = i.strike, t_years = i.t_years, r = i.r, sigma = i.sigma, "computing put price");
    if i.t_years <= 0.0 {
        return (i.strike - i.spot).max(0.0);
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * (i.r * i.t_years).exp();
        let intrinsic = (i.strike - fwd).max(0.0);
        return intrinsic * (-i.r * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    let price = i.strike * (-i.r * i.t_years).exp() * norm_cdf(-d2) - i.spot * norm_cdf(-d1);
    trace!(price, d1, d2, "put price computed");
    price
}

/// Analytic put delta `−N(−d1)` (equivalently `N(d1) − 1`), in `[−1, 0]`. At
/// expiry / zero vol the option is deterministic, so delta is the negative
/// indicator of the (forward) intrinsic being positive.
pub fn put_delta(i: PutInputs) -> f64 {
    if i.t_years <= 0.0 {
        return if i.spot < i.strike { -1.0 } else { 0.0 };
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * (i.r * i.t_years).exp();
        return if fwd < i.strike { -1.0 } else { 0.0 };
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    -norm_cdf(-d1)
}

/// Risk-neutral probability the put finishes in-the-money (S < K) — i.e. the
/// cash-secured writer gets assigned — which is `N(−d2)`. The put analog of
/// `assignment_prob`. Edge conventions match `put_delta`.
pub fn put_assignment_prob(i: PutInputs) -> f64 {
    if i.t_years <= 0.0 {
        return if i.spot < i.strike { 1.0 } else { 0.0 };
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * (i.r * i.t_years).exp();
        return if fwd < i.strike { 1.0 } else { 0.0 };
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    norm_cdf(-d2)
}

/// Analytic Black-Scholes put greeks. `gamma`/`vega` are identical to the
/// call's; `delta` = `N(d1) − 1`; `theta`/`rho` carry the put signs. Same edge
/// conventions as `put_price_per_unit`.
pub fn put_greeks(i: PutInputs) -> Greeks {
    if i.t_years <= 0.0 || i.sigma <= 0.0 {
        return Greeks {
            delta: put_delta(i),
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
            rho: 0.0,
        };
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    let pdf_d1 = norm_pdf(d1);
    let disc = (-i.r * i.t_years).exp();

    let delta = norm_cdf(d1) - 1.0;
    let gamma = pdf_d1 / (i.spot * i.sigma * sqrt_t);
    let vega = i.spot * pdf_d1 * sqrt_t;
    // Annualized put theta, then per-day below.
    let theta_annual =
        -(i.spot * pdf_d1 * i.sigma) / (2.0 * sqrt_t) + i.r * i.strike * disc * norm_cdf(-d2);
    let rho = -i.strike * i.t_years * disc * norm_cdf(-d2);

    Greeks { delta, gamma, vega, theta: theta_annual / 365.0, rho }
}

/// Break-even underlying price for a long put held to expiry:
/// `strike − premium_per_unit`.
pub fn put_break_even(strike: f64, premium_per_unit: f64) -> f64 {
    strike - premium_per_unit
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

/// Standard normal PDF φ(x) = e^(−x²/2) / √(2π).
fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Standard normal CDF: Φ(x) = erfc(−x/√2)/2, ~1 ulp via libm.
fn norm_cdf(x: f64) -> f64 {
    0.5 * libm::erfc(-x / std::f64::consts::SQRT_2)
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
        // norm_cdf is exact to ~1 ulp (libm erfc), so the round trip is
        // bounded by Acklam's ~1.15e-9 relative error in x.
        let mut x = -6.0;
        while x <= 6.0 {
            let p = norm_cdf(x);
            if p > 0.0 && p < 1.0 {
                let x_dx_dp = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
                // Compare in probability space to avoid tail blow-up of dp→dx.
                close(norm_cdf(norm_cdf_inv(p)), p, 1e-12 + 1e-9 / x_dx_dp.max(1e-12));
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
        // Analytic delta vs central finite difference. With the exact
        // (erfc-based) norm_cdf the agreement floor is the FD truncation
        // error itself, so 1e-6 holds.
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
            close(call_delta(i), numeric, 1e-6);
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
    fn assignment_prob_is_below_delta_for_otm_call() {
        // OTM call: N(d2) < N(d1)=delta. The keeper's weekly 0.10-delta strike.
        let (spot, sigma, t): (f64, f64, f64) = (3.5, 0.60, 7.0 / 365.0);
        let k = strike_for_delta(spot, sigma, t, 0.0, 0.10);
        let i = CallInputs { spot, strike: k, t_years: t, r: 0.0, sigma };
        let prob = assignment_prob(i);
        let delta = call_delta(i);
        close(delta, 0.10, 1e-6);
        assert!(prob > 0.0 && prob < delta, "prob {prob} not in (0, {delta})");
    }

    #[test]
    fn assignment_prob_edges() {
        // Expired / zero-vol: indicator on the (forward) intrinsic.
        let expired = CallInputs { spot: 105.0, strike: 100.0, t_years: 0.0, r: 0.0, sigma: 0.5 };
        close(assignment_prob(expired), 1.0, 1e-12);
        close(assignment_prob(CallInputs { spot: 95.0, ..expired }), 0.0, 1e-12);
        let zero_vol = CallInputs { spot: 100.0, strike: 101.0, t_years: 1.0, r: 0.05, sigma: 0.0 };
        close(assignment_prob(zero_vol), 1.0, 1e-12); // fwd ≈ 105.13 > 101
        // ATM with vol: d2 = −σ√t/2 < 0, so just under 0.5.
        let atm = CallInputs { spot: 100.0, strike: 100.0, t_years: 1.0, r: 0.0, sigma: 0.2 };
        let p = assignment_prob(atm);
        assert!(p > 0.45 && p < 0.5, "atm assignment prob {p}");
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

    #[test]
    fn premium_ceil_rounds_up_and_handles_edge_inputs() {
        let p = call_price_per_unit(CallInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.05,
            sigma: 0.20,
        });
        // ~10.4506 per unit: floor pays 10, ceil charges 11.
        assert_eq!(premium_for_write(p, 1), 10);
        assert_eq!(premium_for_write_ceil(p, 1), 11);
        // Exact integers don't get bumped.
        assert_eq!(premium_for_write_ceil(50.0, 2), 100);
        // Degenerate inputs collapse to zero like the floor variant.
        assert_eq!(premium_for_write_ceil(0.0, 100), 0);
        assert_eq!(premium_for_write_ceil(-1.0, 100), 0);
        assert_eq!(premium_for_write_ceil(f64::NAN, 100), 0);
    }

    /// Finite-difference check with a relative floor: FD truncation and f64
    /// cancellation noise scale with the greek's magnitude (and with spot),
    /// so `1e-5 + 1e-5·|expected|` keeps a tight floor on near-zero greeks
    /// (gamma) while admitting proportional noise on big ones (vega/rho at
    /// large spot).
    fn close_greek(a: f64, b: f64) {
        let eps = 1e-5 + 1e-5 * b.abs();
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    #[test]
    fn call_greeks_match_bump_and_reprice() {
        // Analytic greeks vs central finite differences on call_price_per_unit.
        // Tolerance is looser than delta's because gamma is a second
        // derivative and FD noise compounds across the higher-order bumps.
        let cases = [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 130.0, 7.0 / 365.0, 0.0, 0.60),
            (3.5, 4.2, 14.0 / 365.0, 0.03, 0.90),
            (77_000.0, 90_000.0, 30.0 / 365.0, 0.04, 0.45),
        ];
        for (spot, strike, t_years, r, sigma) in cases {
            let i = CallInputs { spot, strike, t_years, r, sigma };
            let g = call_greeks(i);

            // gamma ≈ ∂²price/∂S².
            let hs = spot * 1e-4;
            let p_up = call_price_per_unit(CallInputs { spot: spot + hs, ..i });
            let p_mid = call_price_per_unit(i);
            let p_dn = call_price_per_unit(CallInputs { spot: spot - hs, ..i });
            let gamma_num = (p_up - 2.0 * p_mid + p_dn) / (hs * hs);
            close_greek(g.gamma, gamma_num);

            // vega ≈ ∂price/∂σ.
            let hv = sigma * 1e-4;
            let v_up = call_price_per_unit(CallInputs { sigma: sigma + hv, ..i });
            let v_dn = call_price_per_unit(CallInputs { sigma: sigma - hv, ..i });
            let vega_num = (v_up - v_dn) / (2.0 * hv);
            close_greek(g.vega, vega_num);

            // theta_annual ≈ −∂price/∂τ; greeks.theta is per-day.
            let ht = t_years * 1e-4;
            let t_up = call_price_per_unit(CallInputs { t_years: t_years + ht, ..i });
            let t_dn = call_price_per_unit(CallInputs { t_years: t_years - ht, ..i });
            let theta_annual_num = -(t_up - t_dn) / (2.0 * ht);
            close_greek(g.theta * 365.0, theta_annual_num);

            // rho ≈ ∂price/∂r.
            let hr = 1e-5;
            let r_up = call_price_per_unit(CallInputs { r: r + hr, ..i });
            let r_dn = call_price_per_unit(CallInputs { r: r - hr, ..i });
            let rho_num = (r_up - r_dn) / (2.0 * hr);
            close_greek(g.rho, rho_num);
        }
    }

    #[test]
    fn implied_vol_round_trips() {
        for (spot, strike, t_years, r, sigma) in [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 80.0, 0.5, 0.03, 0.35),  // ITM
            (100.0, 130.0, 0.25, 0.0, 0.60), // OTM, short
            (3.5, 4.2, 14.0 / 365.0, 0.03, 0.90),
            (100.0, 100.0, 2.0, 0.04, 0.15), // long τ
        ] {
            let price = call_price_per_unit(CallInputs { spot, strike, t_years, r, sigma });
            let iv = implied_vol(price, spot, strike, t_years, r)
                .expect("priced call must have an implied vol");
            close(iv, sigma, 1e-6);
        }
    }

    #[test]
    fn implied_vol_screenshot_shape() {
        // Loose regime check against the competitor screenshot: OTM call, rate
        // ignored. The screenshot pins the regime (IV≈0.35, delta≈0.24,
        // theta≈−0.07/day); for the mark 12.78 that regime is reproduced at
        // τ≈0.72y, not the ticket's literal ~1.6 (which would give IV≈0.23,
        // theta≈−0.03 for the same mark). We assert the regime, not figures.
        let iv = implied_vol(12.78, 387.70, 500.0, 0.72, 0.0).expect("should solve");
        assert!((0.33..=0.37).contains(&iv), "iv {iv} out of regime");
        let g = call_greeks(CallInputs {
            spot: 387.70,
            strike: 500.0,
            t_years: 0.72,
            r: 0.0,
            sigma: iv,
        });
        assert!((0.22..=0.26).contains(&g.delta), "delta {} out of regime", g.delta);
        assert!((-0.09..=-0.05).contains(&g.theta), "theta {} out of regime", g.theta);
    }

    #[test]
    fn implied_vol_no_solution() {
        // market ≥ spot.
        assert!(implied_vol(100.0, 100.0, 90.0, 1.0, 0.0).is_none());
        // market below intrinsic (S=110, K=100, r=0 → intrinsic 10).
        assert!(implied_vol(5.0, 110.0, 100.0, 1.0, 0.0).is_none());
        // expiry ≤ 0.
        assert!(implied_vol(5.0, 100.0, 100.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn call_greeks_degenerate() {
        // Expired: smooth greeks vanish, delta is the intrinsic indicator.
        let expired = CallInputs { spot: 105.0, strike: 100.0, t_years: 0.0, r: 0.05, sigma: 0.5 };
        let g = call_greeks(expired);
        close(g.delta, 1.0, 1e-12);
        close(g.gamma, 0.0, 1e-12);
        close(g.vega, 0.0, 1e-12);
        close(g.theta, 0.0, 1e-12);
        close(g.rho, 0.0, 1e-12);

        // Zero vol: indicator on the forward.
        let zero_vol = CallInputs { spot: 100.0, strike: 101.0, t_years: 1.0, r: 0.05, sigma: 0.0 };
        let g = call_greeks(zero_vol);
        close(g.delta, 1.0, 1e-12); // fwd ≈ 105.13 > 101
        close(g.gamma, 0.0, 1e-12);
        close(g.vega, 0.0, 1e-12);
    }

    #[test]
    fn break_even_basic() {
        close(break_even(500.0, 37.15), 537.15, 1e-12);
    }

    // ---- put tests ----

    #[test]
    fn put_call_parity_holds() {
        // C − P = S − K·e^(−rτ) across regimes (the exact relationship).
        let cases = [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 80.0, 0.5, 0.03, 0.35),
            (100.0, 130.0, 0.25, 0.0, 0.60),
            (3.5, 4.2, 14.0 / 365.0, 0.03, 0.90),
            (77_000.0, 90_000.0, 30.0 / 365.0, 0.04, 0.45),
        ];
        for (spot, strike, t_years, r, sigma) in cases {
            let i = CallInputs { spot, strike, t_years, r, sigma };
            let c = call_price_per_unit(i);
            let p = put_price_per_unit(i);
            close(c - p, spot - strike * (-r * t_years).exp(), 1e-6);
        }
    }

    #[test]
    fn put_atm_textbook_value() {
        // S=K=100, T=1, r=0.05, σ=0.20: call≈10.4506, parity ⇒ put≈5.5735.
        let p = put_price_per_unit(PutInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.05,
            sigma: 0.20,
        });
        close(p, 5.5735, 0.01);
    }

    #[test]
    fn put_zero_time_is_intrinsic() {
        // K=100, S=95 → max(K−S,0)=5.
        let p = put_price_per_unit(PutInputs {
            spot: 95.0,
            strike: 100.0,
            t_years: 0.0,
            r: 0.05,
            sigma: 0.6,
        });
        close(p, 5.0, 1e-9);
    }

    #[test]
    fn put_zero_vol_collapses_to_discounted_intrinsic() {
        // σ=0, S=90, K=100, r=0 → max(100−90,0)=10.
        let p = put_price_per_unit(PutInputs {
            spot: 90.0,
            strike: 100.0,
            t_years: 1.0,
            r: 0.0,
            sigma: 0.0,
        });
        close(p, 10.0, 1e-9);
    }

    #[test]
    fn put_delta_matches_bump_and_reprice() {
        let cases = [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 70.0, 7.0 / 365.0, 0.0, 0.60),
            (3.5, 3.0, 14.0 / 365.0, 0.03, 0.90),
            (77_000.0, 60_000.0, 30.0 / 365.0, 0.04, 0.45),
        ];
        for (spot, strike, t_years, r, sigma) in cases {
            let i = PutInputs { spot, strike, t_years, r, sigma };
            let h = spot * 1e-5;
            let up = put_price_per_unit(PutInputs { spot: spot + h, ..i });
            let dn = put_price_per_unit(PutInputs { spot: spot - h, ..i });
            let numeric = (up - dn) / (2.0 * h);
            close(put_delta(i), numeric, 2e-5);
            // delta ∈ [−1, 0] and equals N(d1) − 1 = call_delta − 1.
            assert!(put_delta(i) <= 0.0 && put_delta(i) >= -1.0);
            close(put_delta(i), call_delta(i) - 1.0, 2e-5);
        }
    }

    #[test]
    fn put_delta_edges() {
        // Expired ITM put (S<K) → −1; OTM → 0.
        let base = PutInputs { spot: 95.0, strike: 100.0, t_years: 0.0, r: 0.0, sigma: 0.5 };
        close(put_delta(base), -1.0, 1e-12);
        close(put_delta(PutInputs { spot: 105.0, ..base }), 0.0, 1e-12);
    }

    #[test]
    fn put_assignment_prob_matches_complement() {
        // N(−d2) = 1 − N(d2) = 1 − call assignment_prob (same inputs, smooth regime).
        let i = PutInputs { spot: 100.0, strike: 100.0, t_years: 0.5, r: 0.0, sigma: 0.4 };
        close(put_assignment_prob(i), 1.0 - assignment_prob(i), 1e-9);
        // Expired ITM → 1, OTM → 0.
        let exp = PutInputs { spot: 95.0, strike: 100.0, t_years: 0.0, r: 0.0, sigma: 0.5 };
        close(put_assignment_prob(exp), 1.0, 1e-12);
        close(put_assignment_prob(PutInputs { spot: 105.0, ..exp }), 0.0, 1e-12);
    }

    #[test]
    fn put_greeks_match_bump_and_reprice() {
        let cases = [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 70.0, 7.0 / 365.0, 0.0, 0.60),
            (3.5, 3.0, 14.0 / 365.0, 0.03, 0.90),
        ];
        for (spot, strike, t_years, r, sigma) in cases {
            let i = PutInputs { spot, strike, t_years, r, sigma };
            let g = put_greeks(i);

            let hs = spot * 1e-4;
            let p_up = put_price_per_unit(PutInputs { spot: spot + hs, ..i });
            let p_mid = put_price_per_unit(i);
            let p_dn = put_price_per_unit(PutInputs { spot: spot - hs, ..i });
            close_greek(g.gamma, (p_up - 2.0 * p_mid + p_dn) / (hs * hs));

            let hv = sigma * 1e-4;
            let v_up = put_price_per_unit(PutInputs { sigma: sigma + hv, ..i });
            let v_dn = put_price_per_unit(PutInputs { sigma: sigma - hv, ..i });
            close_greek(g.vega, (v_up - v_dn) / (2.0 * hv));

            let ht = t_years * 1e-4;
            let t_up = put_price_per_unit(PutInputs { t_years: t_years + ht, ..i });
            let t_dn = put_price_per_unit(PutInputs { t_years: t_years - ht, ..i });
            close_greek(g.theta * 365.0, -(t_up - t_dn) / (2.0 * ht));

            let hr = 1e-5;
            let r_up = put_price_per_unit(PutInputs { r: r + hr, ..i });
            let r_dn = put_price_per_unit(PutInputs { r: r - hr, ..i });
            close_greek(g.rho, (r_up - r_dn) / (2.0 * hr));
        }
    }

    #[test]
    fn put_greeks_share_gamma_vega_with_call() {
        // gamma/vega are option-type-independent.
        let i = CallInputs { spot: 100.0, strike: 110.0, t_years: 0.5, r: 0.02, sigma: 0.5 };
        let c = call_greeks(i);
        let p = put_greeks(i);
        close(p.gamma, c.gamma, 1e-12);
        close(p.vega, c.vega, 1e-12);
        close(p.delta, c.delta - 1.0, 1e-12);
    }

    #[test]
    fn put_break_even_basic() {
        close(put_break_even(500.0, 37.15), 462.85, 1e-12);
    }
}
