//! Premium models (doc 06 §4): the mid the market would pay per unit of
//! underlying, in settlement smallest-units per underlying smallest-unit
//! (the same scale-0 cross convention the mm-bot prices in).

use pricing::{call_price_per_unit, CallInputs};

/// Everything needed to price one slice. `spot` and `strike` are scale-0
/// chain crosses (settlement smallest-units per underlying smallest-unit).
#[derive(Clone, Copy, Debug)]
pub struct QuoteCtx {
    pub spot: f64,
    pub strike: f64,
    pub tau_years: f64,
    pub r: f64,
    pub sigma: f64,
}

pub trait PremiumModel {
    /// Mid premium per underlying smallest-unit.
    fn mid(&self, q: &QuoteCtx) -> f64;
}

/// The default: Black-Scholes on the proxy IV — identical math to
/// `mm-bot`'s pricing brain (`crates/pricing`).
pub struct BlackScholesMid;

impl PremiumModel for BlackScholesMid {
    fn mid(&self, q: &QuoteCtx) -> f64 {
        call_price_per_unit(CallInputs {
            spot: q.spot,
            strike: q.strike,
            t_years: q.tau_years,
            r: q.r,
            sigma: q.sigma,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_matches_pricing_crate() {
        let q = QuoteCtx {
            spot: 100.0,
            strike: 100.0,
            tau_years: 1.0,
            r: 0.05,
            sigma: 0.20,
        };
        let mid = BlackScholesMid.mid(&q);
        assert!((mid - 10.4506).abs() < 0.01); // textbook ATM value
    }
}
