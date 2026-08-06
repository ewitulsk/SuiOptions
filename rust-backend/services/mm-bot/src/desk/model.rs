//! Desk pricing adapter — the ONE file that touches `crates/pricing`'s
//! `surface` / `american` / `desk` modules (SO-299).
//!
//! Those modules are being implemented in a parallel workstream; every
//! desk-side use goes through the wrappers here so any signature drift is
//! a one-file fix. Nothing outside `desk/` imports `pricing::surface`,
//! `pricing::american` or `pricing::desk` directly.
//!
//! Units convention (unchanged from the old bot): spot / strike / per-unit
//! prices are settlement-raw per underlying-raw (`compute_spot_from_cache`
//! scale), amounts are underlying raw units, premiums and NAV are
//! settlement raw units.

use std::sync::Arc;

use parking_lot::RwLock;
use pyth_client::RollingVolBuffer;

pub use pricing::american::AmericanInputs;
pub use pricing::Greeks;
pub use pricing::desk::{BidContext, V1BidParams, V2Params, VolDiscount};
use pricing::surface::{SurfaceParams, VolSurface, WindowSample};

/// CRR binomial steps for greeks / exercise-boundary reads. 128 is well
/// past convergence for the tenors the protocol lists (≤ 90d).
pub const CRR_STEPS: usize = 128;

/// Surface shaping knobs (serde-free mirror of `[desk.surface]`).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceConfig {
    pub risk_premium: f64,
    pub skew: f64,
    pub convexity: f64,
    pub term_short_boost: f64,
    pub term_decay_years: f64,
    pub anchor_ratio: Option<f64>,
    pub floor_vol: f64,
    pub cap_vol: f64,
    /// Blend weights for the two realized-vol windows feeding the surface.
    pub short_window_weight: f64,
    pub long_window_weight: f64,
}

/// Per-underlying pricing model: realized-vol windows in, surface + BAW
/// fair values + CRR greeks out.
pub struct MarketModel {
    pub symbol: String,
    /// Canonical underlying coin type (market key).
    pub coin_type: String,
    vol_buf: Arc<RwLock<RollingVolBuffer>>,
    vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    fallback_vol: f64,
    /// Annualized staking yield of the underlying — the BAW dividend rate
    /// (drives early-exercise optimality). 0 for non-yielding assets.
    pub carry_yield: f64,
    /// Risk-free rate; protocol convention is 0.
    pub rate: f64,
    surface_cfg: SurfaceConfig,
}

impl MarketModel {
    pub fn new(
        symbol: String,
        coin_type: String,
        vol_buf: Arc<RwLock<RollingVolBuffer>>,
        vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
        fallback_vol: f64,
        carry_yield: f64,
        rate: f64,
        surface_cfg: SurfaceConfig,
    ) -> Self {
        Self {
            symbol,
            coin_type,
            vol_buf,
            vol_buf_long,
            fallback_vol,
            carry_yield,
            rate,
            surface_cfg,
        }
    }

    /// Build the current vol surface from the two rolling windows.
    pub fn surface(&self) -> VolSurface {
        let c = &self.surface_cfg;
        let windows = [
            WindowSample {
                annualized_vol: self.vol_buf.read().current_annualized(),
                weight: c.short_window_weight,
            },
            WindowSample {
                annualized_vol: self.vol_buf_long.read().current_annualized(),
                weight: c.long_window_weight,
            },
        ];
        let params = SurfaceParams {
            risk_premium: c.risk_premium,
            skew: c.skew,
            convexity: c.convexity,
            term_short_boost: c.term_short_boost,
            term_decay_years: c.term_decay_years,
            anchor_ratio: c.anchor_ratio,
            floor_vol: c.floor_vol,
            cap_vol: c.cap_vol,
        };
        VolSurface::from_windows(&windows, self.fallback_vol, &params)
    }

    /// Surface vol at (spot, strike, t). `is_fallback` when the windows
    /// were cold and the config fallback is quoting.
    pub fn sigma(&self, spot: f64, strike: f64, t_years: f64) -> (f64, bool) {
        let s = self.surface();
        (s.vol(spot, strike, t_years), s.is_fallback())
    }

    /// ATM surface vol at tenor `t` (stress / monitor convenience).
    pub fn atm_sigma(&self, t_years: f64) -> f64 {
        self.surface().atm(t_years)
    }

    /// Current annualized realized vol of the (short, long) windows —
    /// `None` while a window is still cold. `/desk/state` reads these.
    pub fn window_vols(&self) -> (Option<f64>, Option<f64>) {
        (
            self.vol_buf.read().current_annualized(),
            self.vol_buf_long.read().current_annualized(),
        )
    }

    /// Whether the surface is quoting off the config fallback vol
    /// (windows cold).
    pub fn surface_is_fallback(&self) -> bool {
        self.surface().is_fallback()
    }

    fn inputs(&self, spot: f64, strike: f64, t_years: f64, sigma: f64) -> AmericanInputs {
        AmericanInputs {
            spot,
            strike,
            t_years,
            sigma,
            rate: self.rate,
            carry_yield: self.carry_yield,
        }
    }

    /// BAW American per-unit fair value at an explicit sigma (the hot
    /// quoting path).
    pub fn fair_per_unit(
        &self,
        is_put: bool,
        spot: f64,
        strike: f64,
        t_years: f64,
        sigma: f64,
    ) -> f64 {
        let i = self.inputs(spot, strike, t_years, sigma);
        if is_put {
            pricing::american::put_price_baw(&i)
        } else {
            pricing::american::call_price_baw(&i)
        }
    }

    /// Per-unit greeks. Calls use the CRR greeks; puts are bumped BAW
    /// finite differences (the pricing crate only ships call greeks).
    /// Units match `pricing::Greeks`: vega per 1.00 vol, theta per
    /// calendar DAY.
    pub fn greeks_per_unit(
        &self,
        is_put: bool,
        spot: f64,
        strike: f64,
        t_years: f64,
        sigma: f64,
    ) -> Greeks {
        if !is_put {
            let i = self.inputs(spot, strike, t_years, sigma);
            return pricing::american::american_call_greeks(&i, CRR_STEPS);
        }
        if t_years <= 0.0 || sigma <= 0.0 {
            return Greeks {
                delta: if strike > spot { -1.0 } else { 0.0 },
                gamma: 0.0,
                vega: 0.0,
                theta: 0.0,
                rho: 0.0,
            };
        }
        // Central differences on BAW; adequate for risk aggregation.
        let ds = (spot * 0.01).max(1e-12);
        let dv = 1e-4;
        let dt = (t_years * 1e-4).max(1e-12);
        let p = |s: f64, sig: f64, t: f64| {
            let i = self.inputs(s, strike, t.max(0.0), sig);
            pricing::american::put_price_baw(&i)
        };
        let base = p(spot, sigma, t_years);
        let up = p(spot + ds, sigma, t_years);
        let dn = p(spot - ds, sigma, t_years);
        Greeks {
            delta: (up - dn) / (2.0 * ds),
            gamma: (up - 2.0 * base + dn) / (ds * ds),
            vega: (p(spot, sigma + dv, t_years) - p(spot, sigma - dv, t_years)) / (2.0 * dv),
            // Annual θ ÷ 365 to match the crate's per-day convention.
            theta: -((p(spot, sigma, t_years + dt) - p(spot, sigma, t_years - dt)) / (2.0 * dt))
                / 365.0,
            rho: 0.0, // unused by the desk's risk aggregation
        }
    }

    /// Whether early exercise of a held call is CRR-optimal right now.
    pub fn call_exercise_optimal(&self, spot: f64, strike: f64, t_years: f64, sigma: f64) -> bool {
        let i = self.inputs(spot, strike, t_years, sigma);
        pricing::american::call_exercise_optimal_crr(&i, CRR_STEPS)
    }

    /// Remaining (CRR) time value of a held call, per unit.
    pub fn remaining_time_value_call(
        &self,
        spot: f64,
        strike: f64,
        t_years: f64,
        sigma: f64,
    ) -> f64 {
        let i = self.inputs(spot, strike, t_years, sigma);
        pricing::american::remaining_time_value_call(&i, CRR_STEPS)
    }

    /// Carry (staking yield) forgone by holding the option instead of the
    /// underlying, per unit, over the remaining life.
    pub fn forgone_carry(&self, spot: f64, strike: f64, t_years: f64, sigma: f64) -> f64 {
        let i = self.inputs(spot, strike, t_years, sigma);
        pricing::american::forgone_carry(&i)
    }

    /// V1 writer-flow bid for the WHOLE slice (settlement raw units):
    /// model fair at a discounted vol per `pricing::desk::v1_bid`, with
    /// `fair_at` = total premium at a given sigma. `None` = hard decline
    /// (over `max_single_fill_pct_nav`, or net bid ≤ 0). Also returns the
    /// fair sigma used.
    #[allow(clippy::too_many_arguments)]
    pub fn v1_bid_total(
        &self,
        is_put: bool,
        spot: f64,
        strike: f64,
        t_years: f64,
        amount: f64,
        ctx: &BidContext,
        params: &V1BidParams,
    ) -> Option<(f64, f64)> {
        let (sigma, _) = self.sigma(spot, strike, t_years);
        let fair_at = |s: f64| self.fair_per_unit(is_put, spot, strike, t_years, s) * amount;
        pricing::desk::v1_bid(fair_at, sigma, ctx, params).map(|bid| (bid, sigma))
    }

    /// V1 vol-discount decomposition (logging / metrics).
    pub fn v1_vol_discount(&self, ctx: &BidContext, params: &V1BidParams) -> Option<VolDiscount> {
        pricing::desk::v1_vol_discount(ctx, params)
    }
}

/// V2 skewed effective sigmas `(bid_sigma, ask_sigma)` for two-sided
/// quoting. `None` = short-vol capacity exhausted (at/past the short
/// band edge — no writing).
pub fn v2_effective_sigmas(
    sigma_fair: f64,
    net_vega_per_nav_volpt: f64,
    t_years: f64,
    params: &V2Params,
) -> Option<(f64, f64)> {
    pricing::desk::v2_effective_sigmas(sigma_fair, net_vega_per_nav_volpt, t_years, params)
}

/// V2 write-size scale (→ 0 at the short edge of the vega band).
pub fn v2_write_size_scale(net_vega_per_nav_volpt: f64, params: &V2Params) -> f64 {
    pricing::desk::v2_write_size_scale(net_vega_per_nav_volpt, params)
}
