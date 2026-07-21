//! American option pricing with continuous carry — staking yield as the
//! dividend rate — for the mm-bot vol desk (docs/mm-bot-v2/00-plan.md,
//! Phase 1).
//!
//! Two engines over one input struct:
//!
//! - **CRR binomial** (`call_price_crr` / `put_price_crr`) — the
//!   exercise-boundary oracle. Exact up to discretization; use it for the
//!   daily early-exercise check (`call_exercise_optimal_crr`,
//!   `remaining_time_value_call`) and as the reference the fast path is
//!   validated against.
//! - **Barone-Adesi–Whaley** (`call_price_baw` / `put_price_baw`) — the
//!   quadratic approximation for the hot quoting path. Degrades gracefully:
//!   when early exercise can never be optimal (calls with `q <= 0`, puts
//!   with `r <= 0`) or the critical-price Newton iteration fails to
//!   converge, it returns the generalized (carry-adjusted) European
//!   Black-Scholes value.
//!
//! The carry `q` is what makes calls early-exercisable at all: holding the
//! option instead of the underlying forgoes the staking yield, so deep-ITM
//! calls on a high-yield asset are rationally exercised early. Plan §V1
//! item 5 exercises when `forgone_carry > remaining_time_value × 1.1`.
//!
//! Unit conventions match `lib.rs`: prices are per unit of underlying in
//! strike/spot units; `t_years` in years; `sigma`/`rate`/`carry_yield`
//! annualized continuous decimals.

use crate::{norm_cdf, norm_pdf, Greeks};

/// Inputs for the American pricers. `carry_yield` is the continuous
/// dividend/staking yield q (annualized decimal); the risk-neutral drift is
/// `rate − carry_yield`.
#[derive(Clone, Copy, Debug)]
pub struct AmericanInputs {
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    pub sigma: f64,
    pub rate: f64,
    pub carry_yield: f64,
}

impl AmericanInputs {
    fn call_intrinsic(&self) -> f64 {
        (self.spot - self.strike).max(0.0)
    }
    fn put_intrinsic(&self) -> f64 {
        (self.strike - self.spot).max(0.0)
    }
}

// ---- generalized European Black-Scholes (with carry) ----

/// Generalized European call with continuous carry:
/// `S·e^(−qτ)·N(d1) − K·e^(−rτ)·N(d2)` with
/// `d1 = [ln(S/K) + (r − q + σ²/2)τ]/(σ√τ)`. Edge conventions mirror
/// `lib.rs`: expired → intrinsic; zero vol → discounted forward intrinsic
/// on the carry-adjusted forward `S·e^((r−q)τ)`.
pub fn european_call_carry(i: &AmericanInputs) -> f64 {
    if i.t_years <= 0.0 {
        return i.call_intrinsic();
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * ((i.rate - i.carry_yield) * i.t_years).exp();
        return (fwd - i.strike).max(0.0) * (-i.rate * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln()
        + (i.rate - i.carry_yield + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    i.spot * (-i.carry_yield * i.t_years).exp() * norm_cdf(d1)
        - i.strike * (-i.rate * i.t_years).exp() * norm_cdf(d2)
}

/// Generalized European put with continuous carry (parity mirror of
/// [`european_call_carry`]): `K·e^(−rτ)·N(−d2) − S·e^(−qτ)·N(−d1)`.
pub fn european_put_carry(i: &AmericanInputs) -> f64 {
    if i.t_years <= 0.0 {
        return i.put_intrinsic();
    }
    if i.sigma <= 0.0 {
        let fwd = i.spot * ((i.rate - i.carry_yield) * i.t_years).exp();
        return (i.strike - fwd).max(0.0) * (-i.rate * i.t_years).exp();
    }
    let sqrt_t = i.t_years.sqrt();
    let d1 = ((i.spot / i.strike).ln()
        + (i.rate - i.carry_yield + 0.5 * i.sigma * i.sigma) * i.t_years)
        / (i.sigma * sqrt_t);
    let d2 = d1 - i.sigma * sqrt_t;
    i.strike * (-i.rate * i.t_years).exp() * norm_cdf(-d2)
        - i.spot * (-i.carry_yield * i.t_years).exp() * norm_cdf(-d1)
}

// ---- CRR binomial ----

/// Shared CRR backward induction. Returns `(american_price, continuation
/// value at the root)` — the continuation is what the exercise-boundary
/// check compares against immediate intrinsic.
///
/// Standard Cox-Ross-Rubinstein: `u = e^(σ√Δt)`, `d = 1/u`, risk-neutral
/// `p = (e^((r−q)Δt) − d)/(u − d)` (clamped to [0, 1] against extreme
/// drift/Δt combinations), discount `e^(−rΔt)`, early-exercise max at every
/// node. Degenerate inputs (expired, zero vol/spot) collapse to
/// `max(intrinsic, discounted forward intrinsic)` — the deterministic
/// now-or-at-expiry value, matching the `lib.rs` zero-vol convention with
/// an American floor.
fn crr(i: &AmericanInputs, steps: usize, is_call: bool) -> (f64, f64) {
    let payoff = |s: f64| {
        if is_call {
            (s - i.strike).max(0.0)
        } else {
            (i.strike - s).max(0.0)
        }
    };
    let intrinsic = payoff(i.spot);
    if i.t_years <= 0.0 {
        return (intrinsic, intrinsic);
    }
    if i.sigma <= 0.0 || i.spot <= 0.0 || i.strike <= 0.0 {
        let fwd = i.spot * ((i.rate - i.carry_yield) * i.t_years).exp();
        let terminal = payoff(fwd) * (-i.rate * i.t_years).exp();
        return (intrinsic.max(terminal), terminal);
    }

    let n = steps.max(1);
    let dt = i.t_years / n as f64;
    let u = (i.sigma * dt.sqrt()).exp();
    let d = 1.0 / u;
    let disc = (-i.rate * dt).exp();
    let p = ((((i.rate - i.carry_yield) * dt).exp() - d) / (u - d)).clamp(0.0, 1.0);

    // Terminal payoffs: node j has j up-moves out of n.
    let mut v: Vec<f64> = (0..=n)
        .map(|j| payoff(i.spot * u.powi(j as i32) * d.powi((n - j) as i32)))
        .collect();

    let mut root_continuation = 0.0;
    for step in (0..n).rev() {
        for j in 0..=step {
            let s = i.spot * u.powi(j as i32) * d.powi((step - j) as i32);
            let cont = disc * (p * v[j + 1] + (1.0 - p) * v[j]);
            if step == 0 {
                root_continuation = cont;
            }
            v[j] = cont.max(payoff(s));
        }
    }
    (v[0], root_continuation)
}

/// American call price via CRR binomial with `steps` time steps.
pub fn call_price_crr(i: &AmericanInputs, steps: usize) -> f64 {
    crr(i, steps, true).0
}

/// American put price via CRR binomial with `steps` time steps.
pub fn put_price_crr(i: &AmericanInputs, steps: usize) -> f64 {
    crr(i, steps, false).0
}

/// Exercise-boundary oracle: true when exercising the call *now* is at
/// least as good as continuing, per the CRR tree — i.e. immediate intrinsic
/// is positive and ≥ the root continuation value. A worthless (OTM) call is
/// never "optimal to exercise".
pub fn call_exercise_optimal_crr(i: &AmericanInputs, steps: usize) -> bool {
    let (_, continuation) = crr(i, steps, true);
    let intrinsic = i.call_intrinsic();
    intrinsic > 0.0 && intrinsic >= continuation
}

// ---- Barone-Adesi–Whaley ----

/// Relative convergence tolerance for the BAW critical-price iteration.
const BAW_TOL: f64 = 1e-8;
const BAW_MAX_ITERS: usize = 100;

/// American call via the Barone-Adesi–Whaley quadratic approximation. Hot
/// path: closed form plus a short Newton iteration for the critical price.
///
/// - `carry_yield <= 0` (cost of carry b ≥ r): early exercise is never
///   optimal, so the American call *is* the generalized European call —
///   returned exactly.
/// - Newton on the critical price S* is bounded to [`BAW_MAX_ITERS`]; on
///   non-convergence or a degenerate exponent the European value is
///   returned (an underestimate bounded by the early-exercise premium),
///   never NaN.
pub fn call_price_baw(i: &AmericanInputs) -> f64 {
    let euro = european_call_carry(i);
    if i.t_years <= 0.0 || i.sigma <= 0.0 || i.spot <= 0.0 || i.strike <= 0.0 {
        return euro.max(i.call_intrinsic());
    }
    if i.carry_yield <= 0.0 {
        return euro;
    }
    let (t, sig, r) = (i.t_years, i.sigma, i.rate);
    let b = r - i.carry_yield;
    let sig2 = sig * sig;
    let sqrt_t = t.sqrt();
    let ebrt = ((b - r) * t).exp(); // = e^(−qτ)

    let m = 2.0 * r / sig2;
    let n2 = 2.0 * b / sig2;
    // K-factor 1 − e^(−rτ) → rτ as r → 0, so M/K has the finite limit
    // 2/(σ²τ); guard the 0/0 (the protocol prices with r = 0).
    let kk = 1.0 - (-r * t).exp();
    let m_over_kk = if kk.abs() > 1e-12 { m / kk } else { 2.0 / (sig2 * t) };
    let q2 = (-(n2 - 1.0) + ((n2 - 1.0) * (n2 - 1.0) + 4.0 * m_over_kk).sqrt()) / 2.0;
    if !q2.is_finite() || q2 <= 1.0 {
        return euro; // no finite exercise boundary — degrade to European
    }

    // Standard BAW seed: perpetual boundary S_u shrunk toward K.
    let q2u = (-(n2 - 1.0) + ((n2 - 1.0) * (n2 - 1.0) + 4.0 * m).sqrt()) / 2.0;
    let mut si = if q2u > 1.0 {
        let su = i.strike / (1.0 - 1.0 / q2u);
        let h2 = -(b * t + 2.0 * sig * sqrt_t) * i.strike / (su - i.strike);
        i.strike + (su - i.strike) * (1.0 - h2.exp())
    } else {
        i.strike
    };

    let d1_at = |s: f64| ((s / i.strike).ln() + (b + 0.5 * sig2) * t) / (sig * sqrt_t);
    let mut converged = false;
    for _ in 0..BAW_MAX_ITERS {
        let d1 = d1_at(si);
        let c_si = european_call_carry(&AmericanInputs { spot: si, ..*i });
        let lhs = si - i.strike;
        let rhs = c_si + (1.0 - ebrt * norm_cdf(d1)) * si / q2;
        if !rhs.is_finite() {
            break;
        }
        if (lhs - rhs).abs() / i.strike < BAW_TOL {
            converged = true;
            break;
        }
        let bi = ebrt * norm_cdf(d1) * (1.0 - 1.0 / q2)
            + (1.0 - ebrt * norm_pdf(d1) / (sig * sqrt_t)) / q2;
        let next = (i.strike + rhs - bi * si) / (1.0 - bi);
        if !next.is_finite() || next <= 0.0 {
            break;
        }
        si = next;
    }
    if !converged {
        return euro;
    }

    if i.spot >= si {
        return i.call_intrinsic();
    }
    let a2 = (si / q2) * (1.0 - ebrt * norm_cdf(d1_at(si)));
    (euro + a2 * (i.spot / si).powf(q2)).max(euro)
}

/// American put via Barone-Adesi–Whaley. Mirror of [`call_price_baw`]:
/// with `rate <= 0` there is no interest earned on the freed strike, so
/// early exercise is never optimal and the generalized European put is
/// returned exactly; otherwise the critical price S** is found by bounded
/// Newton with a European fallback on non-convergence.
pub fn put_price_baw(i: &AmericanInputs) -> f64 {
    let euro = european_put_carry(i);
    if i.t_years <= 0.0 || i.sigma <= 0.0 || i.spot <= 0.0 || i.strike <= 0.0 {
        return euro.max(i.put_intrinsic());
    }
    if i.rate <= 0.0 {
        return euro;
    }
    let (t, sig, r) = (i.t_years, i.sigma, i.rate);
    let b = r - i.carry_yield;
    let sig2 = sig * sig;
    let sqrt_t = t.sqrt();
    let ebrt = ((b - r) * t).exp();

    let m = 2.0 * r / sig2;
    let n2 = 2.0 * b / sig2;
    let kk = 1.0 - (-r * t).exp();
    let m_over_kk = if kk.abs() > 1e-12 { m / kk } else { 2.0 / (sig2 * t) };
    let q1 = (-(n2 - 1.0) - ((n2 - 1.0) * (n2 - 1.0) + 4.0 * m_over_kk).sqrt()) / 2.0;
    if !q1.is_finite() || q1 >= 0.0 {
        return euro;
    }

    let q1u = (-(n2 - 1.0) - ((n2 - 1.0) * (n2 - 1.0) + 4.0 * m).sqrt()) / 2.0;
    let mut si = if q1u < 0.0 {
        let su = i.strike / (1.0 - 1.0 / q1u);
        let h1 = (b * t - 2.0 * sig * sqrt_t) * i.strike / (i.strike - su);
        su + (i.strike - su) * h1.exp()
    } else {
        i.strike
    };

    let d1_at = |s: f64| ((s / i.strike).ln() + (b + 0.5 * sig2) * t) / (sig * sqrt_t);
    let mut converged = false;
    for _ in 0..BAW_MAX_ITERS {
        if si <= 0.0 {
            break;
        }
        let d1 = d1_at(si);
        let p_si = european_put_carry(&AmericanInputs { spot: si, ..*i });
        let lhs = i.strike - si;
        let rhs = p_si - (1.0 - ebrt * norm_cdf(-d1)) * si / q1;
        if !rhs.is_finite() {
            break;
        }
        if (lhs - rhs).abs() / i.strike < BAW_TOL {
            converged = true;
            break;
        }
        let bi = -ebrt * norm_cdf(-d1) * (1.0 - 1.0 / q1)
            - (1.0 + ebrt * norm_pdf(d1) / (sig * sqrt_t)) / q1;
        let next = (i.strike - rhs + bi * si) / (1.0 + bi);
        if !next.is_finite() || next <= 0.0 {
            break;
        }
        si = next;
    }
    if !converged {
        return euro;
    }

    if i.spot <= si {
        return i.put_intrinsic();
    }
    let a1 = -(si / q1) * (1.0 - ebrt * norm_cdf(-d1_at(si)));
    (euro + a1 * (i.spot / si).powf(q1)).max(euro)
}

// ---- greeks & early-exercise economics ----

/// American call greeks via central finite differences on the CRR price.
/// Units match [`crate::call_greeks`] exactly: vega per 1.00 (=100%) vol,
/// theta per calendar day (annual ÷ 365), rho per 1.00 rate (bumped on
/// `rate` with `carry_yield` held fixed). Spot is bumped ±1% (delta/gamma),
/// sigma ±1e-4, τ ±τ·1e-4 — bumps sized to average over the binomial
/// tree's discretization ripple. Degenerate inputs (expired / zero vol)
/// return the intrinsic-indicator delta and zero smooth greeks, like the
/// analytic functions.
pub fn american_call_greeks(i: &AmericanInputs, steps: usize) -> Greeks {
    if i.t_years <= 0.0 || i.sigma <= 0.0 {
        return Greeks {
            delta: if i.spot > i.strike { 1.0 } else { 0.0 },
            gamma: 0.0,
            vega: 0.0,
            theta: 0.0,
            rho: 0.0,
        };
    }
    let price = |ii: AmericanInputs| call_price_crr(&ii, steps);

    let hs = i.spot * 0.01;
    let p_up = price(AmericanInputs { spot: i.spot + hs, ..*i });
    let p_mid = call_price_crr(i, steps);
    let p_dn = price(AmericanInputs { spot: i.spot - hs, ..*i });
    let delta = (p_up - p_dn) / (2.0 * hs);
    let gamma = (p_up - 2.0 * p_mid + p_dn) / (hs * hs);

    let hv = 1e-4;
    let v_up = price(AmericanInputs { sigma: i.sigma + hv, ..*i });
    let v_dn = price(AmericanInputs { sigma: i.sigma - hv, ..*i });
    let vega = (v_up - v_dn) / (2.0 * hv);

    let ht = i.t_years * 1e-4;
    let t_up = price(AmericanInputs { t_years: i.t_years + ht, ..*i });
    let t_dn = price(AmericanInputs { t_years: i.t_years - ht, ..*i });
    let theta_annual = -(t_up - t_dn) / (2.0 * ht);

    let hr = 1e-4;
    let r_up = price(AmericanInputs { rate: i.rate + hr, ..*i });
    let r_dn = price(AmericanInputs { rate: i.rate - hr, ..*i });
    let rho = (r_up - r_dn) / (2.0 * hr);

    Greeks { delta, gamma, vega, theta: theta_annual / 365.0, rho }
}

/// Time value left in the American call: CRR price − intrinsic, floored at
/// 0 against discretization noise. The §5 early-exercise rule compares
/// `forgone_carry > remaining_time_value × 1.1`.
pub fn remaining_time_value_call(i: &AmericanInputs, steps: usize) -> f64 {
    (call_price_crr(i, steps) - i.call_intrinsic()).max(0.0)
}

/// Carry forgone by holding the option instead of the (staked) underlying
/// over the remaining life: `S·(1 − e^(−qτ))`. Negative when
/// `carry_yield < 0` (there is no yield to forgo — the rule never fires).
pub fn forgone_carry(i: &AmericanInputs) -> f64 {
    i.spot * (1.0 - (-i.carry_yield * i.t_years.max(0.0)).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    fn close_rel(a: f64, b: f64, rel: f64) {
        let denom = b.abs().max(1e-9);
        assert!((a - b).abs() / denom < rel, "{a} vs {b}, rel {rel}");
    }

    /// Standard mildly-dividend-paying ATM case used across the tests.
    fn base() -> AmericanInputs {
        AmericanInputs {
            spot: 100.0,
            strike: 100.0,
            t_years: 0.5,
            sigma: 0.30,
            rate: 0.05,
            carry_yield: 0.07,
        }
    }

    #[test]
    fn crr_matches_european_bs_when_no_carry_no_rate() {
        // q = 0, r = 0: American call = European call = plain lib.rs BS.
        let i = AmericanInputs { rate: 0.0, carry_yield: 0.0, ..base() };
        let euro = crate::call_price_per_unit(crate::CallInputs {
            spot: i.spot,
            strike: i.strike,
            t_years: i.t_years,
            r: 0.0,
            sigma: i.sigma,
        });
        close_rel(call_price_crr(&i, 1000), euro, 1e-3);
        close(european_call_carry(&i), euro, 1e-12);
    }

    #[test]
    fn crr_call_dominates_european_and_intrinsic() {
        let cases = [
            base(),
            AmericanInputs { spot: 130.0, ..base() }, // deep ITM
            AmericanInputs { spot: 80.0, ..base() },  // OTM
            AmericanInputs { carry_yield: 0.15, t_years: 0.1, ..base() },
        ];
        for i in cases {
            let am = call_price_crr(&i, 500);
            assert!(am >= european_call_carry(&i) - 1e-6, "call < European for {i:?}");
            assert!(am >= i.call_intrinsic() - 1e-12, "call < intrinsic for {i:?}");
        }
    }

    #[test]
    fn crr_put_dominates_european_and_intrinsic() {
        let cases = [
            AmericanInputs { carry_yield: 0.0, ..base() },
            AmericanInputs { spot: 80.0, carry_yield: 0.0, ..base() }, // deep ITM put
            AmericanInputs { spot: 120.0, ..base() },
        ];
        for i in cases {
            let am = put_price_crr(&i, 500);
            assert!(am >= european_put_carry(&i) - 1e-6, "put < European for {i:?}");
            assert!(am >= i.put_intrinsic() - 1e-12, "put < intrinsic for {i:?}");
        }
        // r > 0, deep ITM put: the American premium is strictly positive.
        let deep = AmericanInputs { spot: 60.0, carry_yield: 0.0, ..base() };
        assert!(put_price_crr(&deep, 500) > european_put_carry(&deep) + 0.01);
    }

    #[test]
    fn crr_converges_500_vs_1000_steps() {
        for i in [
            base(),
            AmericanInputs { spot: 120.0, carry_yield: 0.12, ..base() },
            AmericanInputs { spot: 85.0, carry_yield: 0.0, ..base() },
        ] {
            close_rel(call_price_crr(&i, 500), call_price_crr(&i, 1000), 2e-3);
            close_rel(put_price_crr(&i, 500), put_price_crr(&i, 1000), 2e-3);
        }
    }

    #[test]
    fn crr_degenerate_inputs() {
        // Expired → intrinsic.
        let i = AmericanInputs { t_years: 0.0, spot: 110.0, ..base() };
        close(call_price_crr(&i, 500), 10.0, 1e-12);
        close(put_price_crr(&AmericanInputs { spot: 90.0, ..i }, 500), 10.0, 1e-12);
        // Zero vol → deterministic now-or-at-expiry value ≥ intrinsic.
        let i = AmericanInputs { sigma: 0.0, spot: 110.0, ..base() };
        let v = call_price_crr(&i, 500);
        assert!(v >= 10.0 - 1e-12, "zero-vol American call below intrinsic: {v}");
    }

    #[test]
    fn early_exercise_flips_with_high_carry() {
        // Deep ITM call, big staking yield, short expiry: continuing forgoes
        // more carry than the remaining time value protects.
        let rich_carry = AmericanInputs {
            spot: 130.0,
            strike: 100.0,
            t_years: 0.1,
            sigma: 0.2,
            rate: 0.0,
            carry_yield: 0.30,
        };
        assert!(call_exercise_optimal_crr(&rich_carry, 500));
        // Same option with zero carry: early exercise is never optimal.
        let no_carry = AmericanInputs { carry_yield: 0.0, ..rich_carry };
        assert!(!call_exercise_optimal_crr(&no_carry, 500));
        // OTM is never "optimal to exercise" no matter the carry.
        let otm = AmericanInputs { spot: 90.0, ..rich_carry };
        assert!(!call_exercise_optimal_crr(&otm, 500));
    }

    #[test]
    fn plan_section5_rule_fires_where_exercise_is_optimal() {
        // Where the boundary oracle says exercise, forgone carry exceeds
        // 1.1× remaining time value (the §5 rule), and vice versa OTM.
        let i = AmericanInputs {
            spot: 130.0,
            strike: 100.0,
            t_years: 0.1,
            sigma: 0.2,
            rate: 0.0,
            carry_yield: 0.30,
        };
        assert!(call_exercise_optimal_crr(&i, 500));
        assert!(forgone_carry(&i) > remaining_time_value_call(&i, 500) * 1.1);
        let calm = AmericanInputs { spot: 100.0, carry_yield: 0.03, t_years: 0.5, ..i };
        assert!(!call_exercise_optimal_crr(&calm, 500));
        assert!(forgone_carry(&calm) < remaining_time_value_call(&calm, 500) * 1.1);
    }

    #[test]
    fn forgone_carry_formula() {
        let i = AmericanInputs { spot: 130.0, carry_yield: 0.30, t_years: 0.1, ..base() };
        close(forgone_carry(&i), 130.0 * (1.0 - (-0.03f64).exp()), 1e-12);
        // No yield → nothing forgone; negative τ treated as 0.
        close(forgone_carry(&AmericanInputs { carry_yield: 0.0, ..i }), 0.0, 1e-12);
        close(forgone_carry(&AmericanInputs { t_years: -1.0, ..i }), 0.0, 1e-12);
    }

    #[test]
    fn baw_call_matches_crr_within_one_percent() {
        let cases = [
            base(),                                                       // ATM, q > r
            AmericanInputs { spot: 110.0, carry_yield: 0.10, t_years: 0.25, sigma: 0.4, rate: 0.03, ..base() },
            AmericanInputs { spot: 90.0, ..base() },                      // OTM
            AmericanInputs { rate: 0.0, carry_yield: 0.05, ..base() },    // the protocol's r = 0
        ];
        for i in cases {
            close_rel(call_price_baw(&i), call_price_crr(&i, 1000), 0.01);
        }
    }

    #[test]
    fn baw_put_matches_crr_within_one_percent() {
        let cases = [
            AmericanInputs { rate: 0.08, carry_yield: 0.0, ..base() },
            AmericanInputs { spot: 90.0, sigma: 0.25, rate: 0.06, carry_yield: 0.02, t_years: 0.25, ..base() },
            AmericanInputs { spot: 110.0, rate: 0.05, carry_yield: 0.0, ..base() },
        ];
        for i in cases {
            close_rel(put_price_baw(&i), put_price_crr(&i, 1000), 0.01);
        }
    }

    #[test]
    fn baw_call_is_exactly_european_without_carry() {
        // q ≤ 0 → American call = European; BAW must return the BS value.
        for q in [0.0, -0.02] {
            let i = AmericanInputs { carry_yield: q, ..base() };
            close(call_price_baw(&i), european_call_carry(&i), 1e-12);
        }
    }

    #[test]
    fn baw_put_is_exactly_european_without_rate() {
        // r ≤ 0 → no interest on the freed strike; American put = European.
        for r in [0.0, -0.01] {
            let i = AmericanInputs { rate: r, carry_yield: 0.03, ..base() };
            close(put_price_baw(&i), european_put_carry(&i), 1e-12);
        }
    }

    #[test]
    fn baw_deep_itm_call_is_intrinsic_past_the_boundary() {
        // Far past the exercise boundary the BAW call is exactly intrinsic,
        // and the CRR tree agrees.
        let i = AmericanInputs {
            spot: 200.0,
            strike: 100.0,
            t_years: 0.5,
            sigma: 0.2,
            rate: 0.02,
            carry_yield: 0.15,
        };
        close(call_price_baw(&i), 100.0, 1e-9);
        close_rel(call_price_crr(&i, 1000), 100.0, 1e-3);
    }

    #[test]
    fn american_greeks_sanity() {
        let g = american_call_greeks(&base(), 500);
        assert!(g.delta > 0.0 && g.delta < 1.0, "delta {}", g.delta);
        assert!(g.vega > 0.0, "vega {}", g.vega);
        assert!(g.theta < 0.0, "ATM theta should bleed: {}", g.theta);
        assert!(g.gamma.is_finite() && g.rho.is_finite());
    }

    #[test]
    fn american_greeks_match_analytic_european_when_no_carry() {
        // q = 0 → American call = European, so the FD greeks must line up
        // with the analytic lib.rs greeks (loose: FD over a discrete tree).
        let i = AmericanInputs { carry_yield: 0.0, ..base() };
        let am = american_call_greeks(&i, 1000);
        let eu = crate::call_greeks(crate::CallInputs {
            spot: i.spot,
            strike: i.strike,
            t_years: i.t_years,
            r: i.rate,
            sigma: i.sigma,
        });
        close(am.delta, eu.delta, 0.02);
        close_rel(am.vega, eu.vega, 0.05);
        close_rel(am.theta, eu.theta, 0.10);
        close_rel(am.rho, eu.rho, 0.05);
    }

    #[test]
    fn american_greeks_degenerate() {
        let expired = AmericanInputs { t_years: 0.0, spot: 110.0, ..base() };
        let g = american_call_greeks(&expired, 500);
        close(g.delta, 1.0, 1e-12);
        close(g.vega, 0.0, 1e-12);
        close(g.theta, 0.0, 1e-12);
        let zero_vol = AmericanInputs { sigma: 0.0, spot: 90.0, ..base() };
        close(american_call_greeks(&zero_vol, 500).delta, 0.0, 1e-12);
    }

    #[test]
    fn remaining_time_value_shrinks_toward_expiry() {
        let long = AmericanInputs { t_years: 0.5, ..base() };
        let short = AmericanInputs { t_years: 0.02, ..base() };
        let tv_long = remaining_time_value_call(&long, 500);
        let tv_short = remaining_time_value_call(&short, 500);
        assert!(tv_long > tv_short, "{tv_long} vs {tv_short}");
        assert!(tv_short >= 0.0);
        // Expired: no time value.
        close(remaining_time_value_call(&AmericanInputs { t_years: 0.0, ..base() }, 500), 0.0, 1e-12);
    }
}
