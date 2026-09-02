//! Constant-flow injector (doc 08 §8 capacity-mode subset): a fixed
//! accepted spot notional per turn or per day, at a configured call/put
//! mix and tenor, with strikes quantised to the LIVE lattice
//! (`pricing::grid::lattice_strikes`) and expiries on the live board
//! (`pricing::grid::expiry_board`) so synthetic writers can only request
//! specs the Earn page would display (doc 09 G13).

use pricing::grid::{expiry_board, lattice_strikes};

use crate::scenario::FlowConfig;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rfq {
    pub is_put: bool,
    pub strike: f64,
    pub expiry_ms: i64,
    /// Underlying units.
    pub qty: f64,
}

/// Nearest lattice strike to the requested moneyness.
pub fn quantised_strike(cfg: &FlowConfig, spot: f64, sigma: f64, tau_years: f64) -> f64 {
    let target = spot * (cfg.moneyness_z * sigma * tau_years.sqrt()).exp();
    let ladder = lattice_strikes(spot, sigma.max(1e-3), tau_years.max(1e-6), cfg.tick_pct, cfg.z_width);
    ladder
        .into_iter()
        .min_by(|a, b| (a - target).abs().partial_cmp(&(b - target).abs()).unwrap())
        .unwrap_or(target)
}

/// The expiry for a fill placed at `now_ms`: the listed board entry
/// closest to the tenor, or the exact tenor when the board is off.
pub fn expiry_for(cfg: &FlowConfig, now_ms: i64) -> i64 {
    let exact = now_ms + (cfg.tenor_days * crate::MS_PER_DAY as f64) as i64;
    if !cfg.use_expiry_board {
        return exact;
    }
    expiry_board(now_ms)
        .into_iter()
        .min_by_key(|e| (e - exact).abs())
        .unwrap_or(exact)
}

/// Split one notional into the configured call/put mix.
pub fn rfqs_for(cfg: &FlowConfig, now_ms: i64, spot: f64, sigma: f64, notional: f64) -> Vec<Rfq> {
    let expiry_ms = expiry_for(cfg, now_ms);
    let tau = (expiry_ms - now_ms) as f64 / crate::MS_PER_YEAR_F;
    let strike = quantised_strike(cfg, spot, sigma, tau);
    let mut out = Vec::new();
    let call_notional = notional * cfg.call_share;
    let put_notional = notional - call_notional;
    if call_notional > 0.0 {
        out.push(Rfq { is_put: false, strike, expiry_ms, qty: call_notional / spot });
    }
    if put_notional > 0.0 {
        out.push(Rfq { is_put: true, strike, expiry_ms, qty: put_notional / spot });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strikes_land_on_the_live_lattice_and_mix_splits_notional() {
        let cfg = FlowConfig { call_share: 0.5, ..Default::default() };
        let rfqs = rfqs_for(&cfg, 0, 3.17, 0.9, 317_000.0);
        assert_eq!(rfqs.len(), 2);
        let tau = 30.0 / 365.0;
        let ladder = lattice_strikes(3.17, 0.9, tau, cfg.tick_pct, cfg.z_width);
        assert!(ladder.contains(&rfqs[0].strike), "{} not on {:?}", rfqs[0].strike, ladder);
        assert!((rfqs[0].qty - 50_000.0).abs() < 1e-6);
        assert!(rfqs[1].is_put);
        // Board mode snaps to a listed weekly/month-end.
        let b = FlowConfig { use_expiry_board: true, ..Default::default() };
        let e = expiry_for(&b, 0);
        assert!(expiry_board(0).contains(&e));
    }
}
