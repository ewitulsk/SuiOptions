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

// ---- carry yield (cost-of-carry generalization) ----
//
// The protocol's options are American (exercise any time before expiry), and
// the underlyings can carry a yield `q` (staking, perp funding). Both matter:
//
//   * For a CALL on a non-yielding underlying (`q = 0`, `r ≥ 0`) early exercise
//     is never optimal, so American == European and `call_price_per_unit` is
//     exact. A positive `q` re-introduces early-exercise value.
//   * For a PUT, early exercise can be optimal whenever the option is deep ITM
//     and `r > 0`, *even at `q = 0`* — so the European `put_price_per_unit`
//     structurally under-prices the American put on a positive rate.
//
// `call_price_carry` / `put_price_carry` are the European prices generalized to
// a continuous carry yield (forward `F = S·e^((r−q)τ)`); the `american_*`
// functions price the early-exercise right via a CRR binomial. All reduce to
// the existing `*_per_unit` functions at `q = 0` (calls) / European (puts).

/// European call price with a continuous carry/dividend yield `q`:
/// `S·e^(−qτ)·N(d1) − K·e^(−rτ)·N(d2)`, `d1 = (ln(S/K)+(r−q+σ²/2)τ)/(σ√τ)`.
/// `q = 0` reproduces [`call_price_per_unit`] exactly.
pub fn call_price_carry(i: CallInputs, q: f64) -> f64 {
    if i.t_years <= 0.0 {
        return (i.spot - i.strike).max(0.0);
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * ((i.r - q) * i.t_years).exp();
        return (fwd - i.strike).max(0.0) * (-i.r * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r - q + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    i.spot * (-q * i.t_years).exp() * norm_cdf(d1) - i.strike * (-i.r * i.t_years).exp() * norm_cdf(d2)
}

/// European put price with a continuous carry/dividend yield `q`:
/// `K·e^(−rτ)·N(−d2) − S·e^(−qτ)·N(−d1)`. `q = 0` reproduces
/// [`put_price_per_unit`] exactly.
pub fn put_price_carry(i: PutInputs, q: f64) -> f64 {
    if i.t_years <= 0.0 {
        return (i.strike - i.spot).max(0.0);
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * ((i.r - q) * i.t_years).exp();
        return (i.strike - fwd).max(0.0) * (-i.r * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r - q + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    i.strike * (-i.r * i.t_years).exp() * norm_cdf(-d2) - i.spot * (-q * i.t_years).exp() * norm_cdf(-d1)
}

/// Default CRR binomial step count. 256 steps prices a weekly-to-monthly option
/// to well under a basis point of the analytic European value — ample for
/// quoting, where the model error dwarfs the tree-discretization error.
pub const AMERICAN_BINOMIAL_STEPS: usize = 256;

/// American option price via a Cox-Ross-Rubinstein binomial tree with carry
/// yield `q`, taking the max of continuation and immediate-exercise value at
/// every node. `is_put = false` prices a call, `true` a put. Degenerates to the
/// raw intrinsic at/after expiry and to the European carry price at zero vol.
///
/// This is the early-exercise-aware pricer: an American call equals its
/// European value when `q = 0` (no early exercise), while an American put on a
/// positive rate is strictly worth more than the European put.
fn american_price(i: CallInputs, q: f64, steps: usize, is_put: bool) -> f64 {
    let intrinsic = |s: f64| {
        if is_put {
            (i.strike - s).max(0.0)
        } else {
            (s - i.strike).max(0.0)
        }
    };
    if i.t_years <= 0.0 {
        return intrinsic(i.spot);
    }
    if i.sigma <= 0.0 || steps == 0 {
        // No volatility ⇒ no exercise optionality beyond the forward; fall back
        // to the closed-form European carry price.
        return if is_put {
            put_price_carry(i, q)
        } else {
            call_price_carry(i, q)
        };
    }
    let n = steps;
    let dt = i.t_years / n as f64;
    let up = (i.sigma * dt.sqrt()).exp();
    let down = 1.0 / up;
    let disc = (-i.r * dt).exp();
    // Risk-neutral up-probability under carry `q`. Guard the rare degenerate
    // regime (huge `r−q` vs tiny σ√dt) by clamping into [0, 1].
    let p = ((((i.r - q) * dt).exp() - down) / (up - down)).clamp(0.0, 1.0);

    // Terminal layer, then fold back, overlaying early exercise at each node.
    let mut values: Vec<f64> = (0..=n)
        .map(|j| intrinsic(i.spot * up.powi(j as i32) * down.powi((n - j) as i32)))
        .collect();
    for step in (0..n).rev() {
        for j in 0..=step {
            let cont = disc * (p * values[j + 1] + (1.0 - p) * values[j]);
            let s = i.spot * up.powi(j as i32) * down.powi((step - j) as i32);
            values[j] = cont.max(intrinsic(s));
        }
    }
    values[0]
}

/// American call price (CRR binomial, [`AMERICAN_BINOMIAL_STEPS`]) with carry
/// yield `q`. Equals [`call_price_per_unit`] when `q = 0`.
pub fn american_call_price(i: CallInputs, q: f64) -> f64 {
    american_price(i, q, AMERICAN_BINOMIAL_STEPS, false)
}

/// American put price (CRR binomial, [`AMERICAN_BINOMIAL_STEPS`]) with carry
/// yield `q`. Captures the early-exercise premium the European
/// [`put_price_per_unit`] omits whenever `r > 0`.
pub fn american_put_price(i: PutInputs, q: f64) -> f64 {
    american_price(i, q, AMERICAN_BINOMIAL_STEPS, true)
}

// ---- implied-vol pipeline (realized σ → pricing IV) ----

/// How a realized-vol estimate is turned into the implied vol used for pricing.
/// The product has no listed IV history, so IV is always a *model*: realized
/// vol lifted by a variance-risk premium and an OTM-wing skew.
///
/// Defaults are the identity transform (`vrp_ratio = 1.0`, both skews `0`), so
/// wiring [`fair_iv`] into a quoter is behaviour-preserving until the knobs are
/// calibrated and set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IvParams {
    /// Variance-risk-premium multiplier (IV/RV). `1.0` = no premium. The vault
    /// keeper already uses ~1.15; this lets every quoter share one number.
    pub vrp_ratio: f64,
    /// Upside-skew slope in bps per unit of log-moneyness, applied to **OTM
    /// calls** (`K > S`): uplift `= call_skew_bps/1e4 · ln(K/S)`. `0` = flat.
    pub call_skew_bps: f64,
    /// Downside-skew slope in bps per unit of log-moneyness, applied to **OTM
    /// puts** (`K < S`): uplift `= put_skew_bps/1e4 · ln(S/K)`. `0` = flat.
    pub put_skew_bps: f64,
}

impl Default for IvParams {
    fn default() -> Self {
        Self { vrp_ratio: 1.0, call_skew_bps: 0.0, put_skew_bps: 0.0 }
    }
}

/// Convert a realized-vol estimate into the implied vol to price with:
/// `IV = RV · vrp_ratio · (1 + skew_uplift)`. Skew lifts only the traded OTM
/// wing (OTM calls for `is_put = false`, OTM puts for `is_put = true`); ITM/ATM
/// and the non-traded wing get no uplift. The result is floored at a small
/// positive value so a degenerate input never yields a zero/negative vol.
pub fn fair_iv(realized_sigma: f64, spot: f64, strike: f64, is_put: bool, p: IvParams) -> f64 {
    let mut iv = realized_sigma * p.vrp_ratio;
    if spot > 0.0 && strike > 0.0 {
        let uplift = if is_put {
            // OTM put: K < S.
            if strike < spot {
                (p.put_skew_bps / 10_000.0) * (spot / strike).ln()
            } else {
                0.0
            }
        } else {
            // OTM call: K > S.
            if strike > spot {
                (p.call_skew_bps / 10_000.0) * (strike / spot).ln()
            } else {
                0.0
            }
        };
        iv *= 1.0 + uplift.max(0.0);
    }
    iv.max(1e-6)
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

/// Standard normal CDF via Graeme West's `cumnorm` (2004) — a Hart-style
/// rational approximation accurate to ~1e-15 absolute, versus the ~7e-8 of the
/// previous Abramowitz-Stegun 26.2.17 form. The tighter accuracy restores the
/// `S·φ(d1) = K·e^(−rτ)·φ(d2)` identity to ~1e-12, so analytic greeks agree
/// with bump-and-reprice to machine-ish precision rather than ~1e-5.
fn norm_cdf(x: f64) -> f64 {
    let x_abs = x.abs();
    // Beyond ~37σ the tail is 0 to full f64 precision.
    if x_abs > 37.0 {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    let exponential = (-x_abs * x_abs / 2.0).exp();
    let cumnorm = if x_abs < 7.071_067_811_865_475 {
        let build = 3.526_249_659_989_109e-2 * x_abs + 0.700_383_064_443_688;
        let build = build * x_abs + 6.373_962_203_531_650;
        let build = build * x_abs + 33.912_866_078_383_02;
        let build = build * x_abs + 112.079_291_497_871_0;
        let build = build * x_abs + 221.213_596_169_931_1;
        let build = build * x_abs + 220.206_867_912_376_1;
        let numer = exponential * build;
        let denom = 8.838_834_764_831_844e-2 * x_abs + 1.755_667_163_182_642;
        let denom = denom * x_abs + 16.064_177_579_206_95;
        let denom = denom * x_abs + 86.780_732_202_946_10;
        let denom = denom * x_abs + 296.564_248_779_674_0;
        let denom = denom * x_abs + 637.333_633_378_831_1;
        let denom = denom * x_abs + 793.826_512_519_948_1;
        let denom = denom * x_abs + 440.413_735_824_752_2;
        numer / denom
    } else {
        // Continued-fraction tail for |x| ≥ ~7.07.
        let build = x_abs + 0.65;
        let build = x_abs + 4.0 / build;
        let build = x_abs + 3.0 / build;
        let build = x_abs + 2.0 / build;
        let build = x_abs + 1.0 / build;
        exponential / build / 2.506_628_274_631_000
    };
    if x > 0.0 {
        1.0 - cumnorm
    } else {
        cumnorm
    }
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

    /// Finite-difference check with a relative floor: the A-S `norm_cdf`'s
    /// ~7e-8 absolute error scales with price (and thus with spot), so a
    /// large-spot vega/rho carries proportionally larger FD noise than the
    /// flat 2e-4 a small fixture allows. `2e-4 + 2e-4·|expected|` keeps the
    /// tight floor on near-zero greeks (gamma) while admitting it on big ones.
    fn close_greek(a: f64, b: f64) {
        let eps = 2e-4 + 2e-4 * b.abs();
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    #[test]
    fn call_greeks_match_bump_and_reprice() {
        // Analytic greeks vs central finite differences on call_price_per_unit.
        // Tolerance is looser than delta's (2e-5) because gamma is a second
        // derivative and the A-S norm_cdf error compounds across the higher-
        // order bumps.
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

    // ---- higher-accuracy norm_cdf (West) ----

    #[test]
    fn norm_cdf_is_high_accuracy() {
        // West's algorithm is ~1e-15; pin reference values much tighter than
        // the old A-S 26.2.17 (~7e-8) form allowed.
        close(norm_cdf(0.0), 0.5, 1e-15);
        close(norm_cdf(1.0), 0.841_344_746_068_543, 1e-12);
        close(norm_cdf(-1.0), 0.158_655_253_931_457, 1e-12);
        close(norm_cdf(1.96), 0.975_002_104_851_780, 1e-12);
        close(norm_cdf(-3.0), 0.001_349_898_031_630, 1e-12);
        // Symmetry and tail saturation.
        close(norm_cdf(5.0) + norm_cdf(-5.0), 1.0, 1e-15);
        assert_eq!(norm_cdf(40.0), 1.0);
        assert_eq!(norm_cdf(-40.0), 0.0);
    }

    #[test]
    fn norm_cdf_restores_phi_identity() {
        // The accuracy gain restores S·φ(d1) = K·e^(−rτ)·φ(d2), so analytic
        // call delta now matches central differences to ~1e-9 (was ~1e-5).
        let i = CallInputs { spot: 100.0, strike: 130.0, t_years: 7.0 / 365.0, r: 0.0, sigma: 0.60 };
        let h = i.spot * 1e-6;
        let up = call_price_per_unit(CallInputs { spot: i.spot + h, ..i });
        let dn = call_price_per_unit(CallInputs { spot: i.spot - h, ..i });
        close(call_delta(i), (up - dn) / (2.0 * h), 1e-7);
    }

    // ---- carry-aware European prices ----

    #[test]
    fn carry_reduces_to_european_at_zero_q() {
        for (spot, strike, t_years, r, sigma) in [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 130.0, 0.25, 0.0, 0.60),
            (3.5, 4.2, 14.0 / 365.0, 0.03, 0.90),
        ] {
            let i = CallInputs { spot, strike, t_years, r, sigma };
            close(call_price_carry(i, 0.0), call_price_per_unit(i), 1e-12);
            close(put_price_carry(i, 0.0), put_price_per_unit(i), 1e-12);
        }
    }

    #[test]
    fn carry_yield_lowers_call_raises_put() {
        // A positive carry yield drops the forward, so the call is worth less
        // and the put more than their q=0 values.
        let i = CallInputs { spot: 100.0, strike: 100.0, t_years: 1.0, r: 0.02, sigma: 0.40 };
        assert!(call_price_carry(i, 0.05) < call_price_per_unit(i));
        assert!(put_price_carry(i, 0.05) > put_price_per_unit(i));
    }

    #[test]
    fn carry_put_call_parity_with_yield() {
        // C − P = S·e^(−qτ) − K·e^(−rτ).
        let i = CallInputs { spot: 100.0, strike: 110.0, t_years: 0.5, r: 0.03, sigma: 0.35 };
        let q = 0.04;
        let lhs = call_price_carry(i, q) - put_price_carry(i, q);
        let rhs = i.spot * (-q * i.t_years).exp() - i.strike * (-i.r * i.t_years).exp();
        close(lhs, rhs, 1e-9);
    }

    // ---- American binomial pricer ----

    #[test]
    fn american_call_equals_european_when_no_carry() {
        // No dividend ⇒ early exercise never optimal ⇒ American == European.
        for (spot, strike, t_years, r, sigma) in [
            (100.0, 100.0, 1.0, 0.05, 0.20),
            (100.0, 130.0, 30.0 / 365.0, 0.04, 0.55),
            (3.5, 4.0, 7.0 / 365.0, 0.03, 0.90),
        ] {
            let i = CallInputs { spot, strike, t_years, r, sigma };
            // Tree vs analytic European: a few bps of spot at 256 steps.
            close(american_call_price(i, 0.0), call_price_per_unit(i), 0.02 * spot.max(1.0) / 100.0 + 1e-3);
        }
    }

    #[test]
    fn american_put_exceeds_european_on_positive_rate() {
        // The headline put fix: with r > 0 the American put carries a strictly
        // positive early-exercise premium the European model omits. Use a deep
        // ITM put where early exercise is clearly valuable.
        let i = PutInputs { spot: 60.0, strike: 100.0, t_years: 1.0, r: 0.10, sigma: 0.25 };
        let euro = put_price_per_unit(i);
        let amer = american_put_price(i, 0.0);
        assert!(amer > euro + 0.5, "amer {amer} should beat euro {euro} by the exercise premium");
        // It can never be worth less than immediate exercise.
        assert!(amer >= i.strike - i.spot - 1e-6);
    }

    #[test]
    fn american_put_matches_european_at_zero_rate_zero_carry() {
        // With r = q = 0 there is no early-exercise incentive for a put either.
        let i = PutInputs { spot: 95.0, strike: 100.0, t_years: 0.5, r: 0.0, sigma: 0.40 };
        close(american_put_price(i, 0.0), put_price_per_unit(i), 0.02);
    }

    #[test]
    fn american_degenerate_inputs() {
        // Expired ⇒ raw intrinsic; zero vol ⇒ European carry price.
        let exp = CallInputs { spot: 110.0, strike: 100.0, t_years: 0.0, r: 0.05, sigma: 0.4 };
        close(american_call_price(exp, 0.0), 10.0, 1e-12);
        close(american_put_price(CallInputs { spot: 90.0, ..exp }, 0.0), 10.0, 1e-12);
        let zero_vol = CallInputs { spot: 100.0, strike: 100.0, t_years: 1.0, r: 0.05, sigma: 0.0 };
        close(american_call_price(zero_vol, 0.0), call_price_carry(zero_vol, 0.0), 1e-12);
    }

    // ---- fair_iv pipeline ----

    #[test]
    fn fair_iv_default_is_identity() {
        // Defaults must not move pricing: IV == RV regardless of moneyness/type.
        let p = IvParams::default();
        close(fair_iv(0.55, 100.0, 120.0, false, p), 0.55, 1e-12);
        close(fair_iv(0.55, 100.0, 80.0, true, p), 0.55, 1e-12);
    }

    #[test]
    fn fair_iv_applies_vrp() {
        let p = IvParams { vrp_ratio: 1.15, ..Default::default() };
        close(fair_iv(0.50, 100.0, 100.0, false, p), 0.575, 1e-12);
    }

    #[test]
    fn fair_iv_skew_lifts_only_traded_otm_wing() {
        // OTM call (K>S) gets the call-skew uplift; an ITM call does not, and
        // the put-skew knob leaves calls untouched.
        let call_p = IvParams { vrp_ratio: 1.0, call_skew_bps: 5000.0, put_skew_bps: 9999.0 };
        let otm_call = fair_iv(0.50, 100.0, 130.0, false, call_p);
        let itm_call = fair_iv(0.50, 100.0, 80.0, false, call_p);
        assert!(otm_call > 0.50, "otm call iv {otm_call} should be lifted");
        close(itm_call, 0.50, 1e-12);

        // OTM put (K<S) gets the put-skew uplift; ITM put does not.
        let put_p = IvParams { vrp_ratio: 1.0, call_skew_bps: 9999.0, put_skew_bps: 5000.0 };
        let otm_put = fair_iv(0.50, 100.0, 70.0, true, put_p);
        let itm_put = fair_iv(0.50, 100.0, 130.0, true, put_p);
        assert!(otm_put > 0.50, "otm put iv {otm_put} should be lifted");
        close(itm_put, 0.50, 1e-12);
    }

    #[test]
    fn fair_iv_floors_positive() {
        // A degenerate zero RV still yields a usable positive vol.
        assert!(fair_iv(0.0, 100.0, 100.0, false, IvParams::default()) > 0.0);
    }
}
