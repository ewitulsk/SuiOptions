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
//!
//! Vol estimator (SO-440): `[desk.surface] estimator` picks between the
//! two-window blend (`"windows"`, the default) and the `vol-forecast`
//! HAR-RV forecaster (`"har"`). Either way the model keeps a long price
//! history and runs the forecaster on it, so `/desk/state` shows the
//! shadow forecast while `"windows"` still quotes; `"har"` quotes
//! `quantile(q_bid)` and falls back to the windows blend whenever the
//! forecast is cold or unusable.

use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use vol_forecast::{
    Calibration, ForecastConfig, ForecastInput, Horizon, PriceHistory, Regime, RollingVolBuffer,
    VolForecast,
};

pub use pricing::american::AmericanInputs;
pub use pricing::desk::{BidContext, V1BidParams, VolDiscount};
use pricing::surface::{SurfaceParams, VolSurface, WindowSample};
pub use pricing::Greeks;

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

/// Which estimator sets the surface's ATM sigma (`[desk.surface] estimator`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EstimatorKind {
    /// The max-leaning two-window realized-vol blend (`VolSurface::from_windows`).
    Windows,
    /// The `vol-forecast` HAR-RV forecaster at `q_bid` (`VolSurface::from_forecast`).
    Har,
}

impl EstimatorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EstimatorKind::Windows => "windows",
            EstimatorKind::Har => "har",
        }
    }
}

/// Forecaster wiring (serde-free mirror of the estimator half of
/// `[desk.surface]`).
#[derive(Clone, Copy, Debug)]
pub struct EstimatorConfig {
    pub kind: EstimatorKind,
    /// Forecast quantile the bid is priced at (doc 09 §2.2 item 5).
    pub q_bid: f64,
    /// Refit cadence for the HAR calibration.
    pub refit_secs: u64,
    /// Forecast horizon: the desk's expected holding period.
    pub horizon_ms: u64,
    /// Derive wing convexity from the asset's own kurtosis (`"har"` only).
    pub convexity_from_kurtosis: bool,
}

/// What `/desk/state` shows about the estimator (SO-440).
#[derive(Clone, Debug)]
pub struct EstimatorState {
    /// Which estimator quotes the surface.
    pub estimator: &'static str,
    pub regime: Option<String>,
    pub sample_interval_ms: Option<u64>,
    pub sigma_mean: Option<f64>,
    /// `quantile(q_bid)` of the forecast.
    pub sigma_q_bid: Option<f64>,
}

struct Estimator {
    cfg: EstimatorConfig,
    forecast_cfg: ForecastConfig,
    history: Arc<RwLock<PriceHistory>>,
    calibration: RwLock<Option<Calibration>>,
    /// Last forecast, keyed by the newest history timestamp it saw.
    cached: RwLock<Option<(u64, Arc<VolForecast>)>>,
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
    estimator: Option<Estimator>,
}

impl MarketModel {
    #[allow(clippy::too_many_arguments)]
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
            estimator: None,
        }
    }

    /// Attach the long price history the forecaster reads (sized by
    /// `ForecastConfig::required_history_ms`) and the estimator config.
    pub fn with_estimator(
        mut self,
        history: Arc<RwLock<PriceHistory>>,
        cfg: EstimatorConfig,
    ) -> Self {
        self.estimator = Some(Estimator {
            cfg,
            forecast_cfg: ForecastConfig::default(),
            history,
            calibration: RwLock::new(None),
            cached: RwLock::new(None),
        });
        self
    }

    fn params(&self) -> SurfaceParams {
        let c = &self.surface_cfg;
        SurfaceParams {
            risk_premium: c.risk_premium,
            skew: c.skew,
            convexity: c.convexity,
            term_short_boost: c.term_short_boost,
            term_decay_years: c.term_decay_years,
            anchor_ratio: c.anchor_ratio,
            floor_vol: c.floor_vol,
            cap_vol: c.cap_vol,
            convexity_from_kurtosis: self
                .estimator
                .as_ref()
                .is_some_and(|e| e.cfg.convexity_from_kurtosis),
        }
    }

    fn windows_surface(&self) -> VolSurface {
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
        VolSurface::from_windows(&windows, self.fallback_vol, &self.params())
    }

    /// Current forecast from the attached history: cached per newest
    /// sample, calibration refit on the `refit_secs` schedule. "Now" is
    /// the newest sample's timestamp, so this stays clock-free and
    /// byte-identical to a backtest replay of the same history.
    fn har_forecast(&self) -> Option<Arc<VolForecast>> {
        let est = self.estimator.as_ref()?;
        let history = est.history.read();
        let last = history.last_ts()?;
        if let Some((ts, fc)) = &*est.cached.read() {
            if *ts == last {
                return Some(Arc::clone(fc));
            }
        }
        let input = ForecastInput {
            asset: &self.symbol,
            history: history.samples(),
        };
        let mut cal = est.calibration.write();
        let due = cal
            .as_ref()
            .is_none_or(|c| c.is_due(last, est.cfg.refit_secs.saturating_mul(1000)));
        if due {
            *cal = Some(vol_forecast::fit(
                &est.forecast_cfg,
                &input,
                Horizon::from_ms(est.cfg.horizon_ms),
            ));
        }
        let fc = Arc::new(vol_forecast::forecast(cal.as_ref()?, &input, last));
        *est.cached.write() = Some((last, Arc::clone(&fc)));
        Some(fc)
    }

    /// Build the current vol surface: the forecast at `q_bid` when the
    /// estimator is `"har"` and the forecast is warm, else the two-window
    /// blend.
    pub fn surface(&self) -> VolSurface {
        if let Some(est) = &self.estimator {
            if est.cfg.kind == EstimatorKind::Har {
                if let Some(fc) = self.har_forecast() {
                    if fc.is_usable() && fc.regime != Regime::Cold {
                        return VolSurface::from_forecast(&fc, est.cfg.q_bid, &self.params());
                    }
                }
            }
        }
        self.windows_surface()
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

    /// Estimator selection plus the (live or shadow) forecast for
    /// `/desk/state`. Forecast fields are `None` when no history is
    /// attached or it is still empty.
    pub fn estimator_state(&self) -> EstimatorState {
        let (estimator, q_bid) = match &self.estimator {
            Some(e) => (e.cfg.kind.as_str(), e.cfg.q_bid),
            None => (EstimatorKind::Windows.as_str(), 0.0),
        };
        let fc = self.har_forecast();
        EstimatorState {
            estimator,
            regime: fc.as_ref().map(|f| f.regime.to_string()),
            sample_interval_ms: fc.as_ref().map(|f| f.sample_interval_ms),
            sigma_mean: fc.as_ref().filter(|f| f.is_usable()).map(|f| f.sigma_mean),
            sigma_q_bid: fc
                .as_ref()
                .filter(|f| f.is_usable())
                .map(|f| f.quantile(q_bid)),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use vol_forecast::synthetic::{sv_jump_path, SvJumpParams};

    const DAY_MS: u64 = 86_400_000;

    fn cfg() -> SurfaceConfig {
        SurfaceConfig {
            risk_premium: 0.0,
            skew: 0.0,
            convexity: 0.0,
            term_short_boost: 0.0,
            term_decay_years: 0.25,
            anchor_ratio: None,
            floor_vol: 0.01,
            cap_vol: 5.0,
            short_window_weight: 1.0,
            long_window_weight: 1.0,
        }
    }

    fn model(kind: EstimatorKind, history: Arc<RwLock<PriceHistory>>) -> MarketModel {
        MarketModel::new(
            "TSUI".into(),
            "0x1::tsui::TSUI".into(),
            Arc::new(RwLock::new(RollingVolBuffer::new(DAY_MS))),
            Arc::new(RwLock::new(RollingVolBuffer::new(7 * DAY_MS))),
            0.60,
            0.0,
            0.0,
            cfg(),
        )
        .with_estimator(
            history,
            EstimatorConfig {
                kind,
                q_bid: 0.35,
                refit_secs: 86_400,
                horizon_ms: 21 * DAY_MS,
                convexity_from_kurtosis: false,
            },
        )
    }

    fn warm_history(days: u32) -> Arc<RwLock<PriceHistory>> {
        let path = sv_jump_path(
            3,
            &SvJumpParams {
                days,
                interval_ms: 300_000,
                ..Default::default()
            },
        );
        let mut h = PriceHistory::new(ForecastConfig::default().required_history_ms());
        for (t, p) in path.history {
            h.push(t, p);
        }
        Arc::new(RwLock::new(h))
    }

    #[test]
    fn windows_estimator_quotes_the_blend_and_shadows_the_forecast() {
        let m = model(EstimatorKind::Windows, warm_history(120));
        // Windows are cold → fallback 0.60 quotes, regardless of the forecast.
        let s = m.surface();
        assert!(s.is_fallback());
        assert!((s.atm(0.05) - 0.60).abs() < 1e-12);
        let st = m.estimator_state();
        assert_eq!(st.estimator, "windows");
        assert_eq!(st.regime.as_deref(), Some("calm"));
        assert_eq!(st.sample_interval_ms, Some(300_000));
        let (mean, q) = (st.sigma_mean.unwrap(), st.sigma_q_bid.unwrap());
        assert!(q < mean && (mean / 0.87 - 1.0).abs() < 0.4, "{st:?}");
    }

    #[test]
    fn har_estimator_quotes_the_bid_quantile_and_falls_back_while_cold() {
        let m = model(EstimatorKind::Har, warm_history(120));
        let s = m.surface();
        assert!(!s.is_fallback());
        let st = m.estimator_state();
        assert_eq!(st.estimator, "har");
        assert!((s.atm(0.05) - st.sigma_q_bid.unwrap()).abs() < 1e-12);
        // Same history twice → identical surface (cache + determinism).
        assert_eq!(m.surface(), s);

        // Too little history: Cold → the windows blend (here: fallback).
        let m = model(EstimatorKind::Har, warm_history(10));
        let s = m.surface();
        assert!(s.is_fallback());
        assert!((s.atm(0.05) - 0.60).abs() < 1e-12);
        assert_eq!(m.estimator_state().regime.as_deref(), Some("cold"));

        // No samples at all: nothing to forecast, windows path.
        let m = model(
            EstimatorKind::Har,
            Arc::new(RwLock::new(PriceHistory::new(DAY_MS))),
        );
        assert!(m.surface().is_fallback());
        let st = m.estimator_state();
        assert!(st.regime.is_none() && st.sigma_mean.is_none());
    }

    #[test]
    fn calibration_refits_on_schedule_not_per_sample() {
        let history = warm_history(120);
        let m = model(EstimatorKind::Har, Arc::clone(&history));
        let _ = m.surface();
        let fitted_at = m
            .estimator
            .as_ref()
            .unwrap()
            .calibration
            .read()
            .as_ref()
            .unwrap()
            .fitted_at_ms;
        let last = history.read().last_ts().unwrap();
        // A new sample inside the refit window: forecast recomputed, fit kept.
        history.write().push(last + 300_000, 1.0);
        let _ = m.surface();
        let est = m.estimator.as_ref().unwrap();
        assert_eq!(
            est.calibration.read().as_ref().unwrap().fitted_at_ms,
            fitted_at
        );
        assert_eq!(est.cached.read().as_ref().unwrap().0, last + 300_000);
        // Past the schedule: refit.
        history.write().push(last + 86_400_000 + 1, 1.0);
        let _ = m.surface();
        assert_eq!(
            est.calibration.read().as_ref().unwrap().fitted_at_ms,
            last + 86_400_000 + 1
        );
    }
}
