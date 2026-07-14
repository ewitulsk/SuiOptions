//! `GET /options/metrics` — Black-Scholes greeks, implied vol, fair value,
//! and break-even for a single call. STATELESS: every input is a query
//! param, so the service stays a pure compute endpoint (no Pyth / venue
//! reads). The frontend holds live spot and the observed option mark and
//! passes them in. Ported verbatim from the Sui twin — the `pricing` crate
//! is chain-agnostic.
//!
//! UNITS: `spot`, `strike`, and `mark` MUST share one unit (e.g. settlement
//! per option, whole units). `t_years` is calendar time to expiry in years.
//! `r` is the annualized continuous risk-free rate; omit for the default.

use axum::{extract::Query, Json};
use serde::{Deserialize, Serialize};

use pricing::{break_even, call_greeks, call_price_per_unit, implied_vol, CallInputs};

/// Default risk-free rate when `r` is omitted. 0 keeps greeks comparable
/// to a retail screen that ignores rates.
const DEFAULT_RATE: f64 = 0.0;

#[derive(Deserialize)]
pub struct MetricsQuery {
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    /// Observed option price (order-book mid, or avg cost for break-even),
    /// same unit as spot/strike.
    pub mark: f64,
    pub r: Option<f64>,
}

#[derive(Serialize)]
pub struct MetricsResponse {
    /// Annualized vol, e.g. 0.3492 for 34.92%. `null` if no solution
    /// (mark below intrinsic, ≥ spot, or expiry ≤ 0).
    pub implied_vol: Option<f64>,
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    /// Per-calendar-day theta.
    pub theta: f64,
    pub rho: f64,
    pub break_even: f64,
    /// BS price re-priced at the solved IV — a sanity echo of `mark`.
    /// `null` when there is no IV.
    pub fair_value: Option<f64>,
}

pub async fn option_metrics(Query(q): Query<MetricsQuery>) -> Json<MetricsResponse> {
    let r = q.r.unwrap_or(DEFAULT_RATE);
    let iv = implied_vol(q.mark, q.spot, q.strike, q.t_years, r);

    // Greeks at the solved IV. With no IV (degenerate expiry / out-of-band
    // mark) fall back to sigma=0 so the row still renders (greeks collapse
    // to the deterministic case).
    let sigma = iv.unwrap_or(0.0);
    let inputs = CallInputs { spot: q.spot, strike: q.strike, t_years: q.t_years, r, sigma };
    let g = call_greeks(inputs);
    let fair_value = iv.map(|s| call_price_per_unit(CallInputs { sigma: s, ..inputs }));

    Json(MetricsResponse {
        implied_vol: iv,
        delta: g.delta,
        gamma: g.gamma,
        vega: g.vega,
        theta: g.theta,
        rho: g.rho,
        break_even: break_even(q.strike, q.mark),
        fair_value,
    })
}
