//! Fair value and greeks for one option at one sigma — BAW American
//! pricing from `pricing::american`, greeks by central differences
//! (the desk's `MarketModel` does the same).

use pricing::american::{call_price_baw, put_price_baw, AmericanInputs};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    /// Per 1.00 vol.
    pub vega: f64,
    /// Per calendar day, negative for a long option.
    pub theta: f64,
}

pub fn fair_per_unit(is_put: bool, spot: f64, strike: f64, t_years: f64, sigma: f64, carry: f64) -> f64 {
    let i = AmericanInputs { spot, strike, t_years: t_years.max(0.0), sigma: sigma.max(1e-6), rate: 0.0, carry_yield: carry };
    if is_put { put_price_baw(&i) } else { call_price_baw(&i) }
}

pub fn greeks_per_unit(is_put: bool, spot: f64, strike: f64, t_years: f64, sigma: f64, carry: f64) -> Greeks {
    if t_years <= 0.0 || sigma <= 0.0 {
        let itm = if is_put { spot < strike } else { spot > strike };
        let delta = if !itm { 0.0 } else if is_put { -1.0 } else { 1.0 };
        return Greeks { delta, ..Default::default() };
    }
    let p = |s: f64, v: f64, t: f64| fair_per_unit(is_put, s, strike, t, v, carry);
    let ds = spot * 1e-3;
    let dv = 1e-3;
    let dt = (1.0_f64 / 365.0).min(t_years / 2.0).max(1e-6);
    let base = p(spot, sigma, t_years);
    let up = p(spot + ds, sigma, t_years);
    let dn = p(spot - ds, sigma, t_years);
    Greeks {
        delta: (up - dn) / (2.0 * ds),
        gamma: (up - 2.0 * base + dn) / (ds * ds),
        vega: (p(spot, sigma + dv, t_years) - p(spot, sigma - dv, t_years)) / (2.0 * dv),
        theta: -((p(spot, sigma, t_years + dt) - p(spot, sigma, t_years - dt)) / (2.0 * dt)) / 365.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_are_sane() {
        let c = greeks_per_unit(false, 100.0, 100.0, 30.0 / 365.0, 0.8, 0.0);
        let p = greeks_per_unit(true, 100.0, 100.0, 30.0 / 365.0, 0.8, 0.0);
        assert!(c.delta > 0.4 && c.delta < 0.7, "{c:?}");
        assert!(p.delta < -0.3 && p.delta > -0.6, "{p:?}");
        assert!(c.gamma > 0.0 && p.gamma > 0.0);
        assert!(c.vega > 0.0 && p.vega > 0.0);
        assert!(c.theta < 0.0 && p.theta < 0.0);
        // Expired: intrinsic delta only.
        assert_eq!(greeks_per_unit(false, 110.0, 100.0, 0.0, 0.8, 0.0).delta, 1.0);
    }
}
