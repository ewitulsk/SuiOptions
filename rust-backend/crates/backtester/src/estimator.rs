//! The bid's vol input. v0 reproduces the live desk's two-window blend
//! through `pricing::surface` so the G6 study can measure it; the HAR
//! forecaster (G5, `vol-forecast`) plugs in behind the same trait once
//! it lands.

use pricing::surface::{SurfaceParams, VolSurface, WindowSample};
use vol_forecast::{Calibration, ForecastConfig, ForecastInput, Horizon};

use crate::scenario::EstimatorConfig;

/// Realized vol over a window of (ts_ms, price) samples taken every
/// `interval_ms`: close-to-close log returns, annualized by the actual
/// sample spacing so gaps do not bias it.
pub fn realized_vol(samples: &[(i64, f64)], window_ms: i64, now_ms: i64) -> Option<f64> {
    let start = now_ms - window_ms;
    let pts: Vec<(i64, f64)> = samples.iter().copied().filter(|(t, _)| *t > start && *t <= now_ms).collect();
    if pts.len() < 8 {
        return None;
    }
    let mut sum = 0.0;
    for w in pts.windows(2) {
        let r = (w[1].1 / w[0].1).ln();
        sum += r * r;
    }
    let n = (pts.len() - 1) as f64;
    let span_years = (pts[pts.len() - 1].0 - pts[0].0) as f64 / crate::MS_PER_YEAR_F;
    if span_years <= 0.0 {
        return None;
    }
    // Variance per sample × samples per year.
    Some((sum / n * (n / span_years)).sqrt())
}

pub struct WindowsEstimator {
    cfg: EstimatorConfig,
    /// Sampled decision prices at the derived interval.
    samples: Vec<(i64, f64)>,
    last_sample_ms: i64,
    /// Latest vol-index reading (annualized decimal) for `kind =
    /// "vol_index"`; the engine sets it from the LOCF series.
    index_vol: Option<f64>,
    /// `har`: every decision price (the forecaster derives its own
    /// sampling interval from the raw cadence), bounded to the
    /// calibration window.
    raw: Vec<(u64, f64)>,
    har_cfg: ForecastConfig,
    horizon: Horizon,
    calibration: Option<Calibration>,
    last_fit_ms: i64,
    /// Cached forecast readout (refreshed at the sample cadence).
    har_surface: Option<VolSurface>,
    last_har_ms: i64,
    pub har_regime: Option<String>,
    pub har_sigma_mean: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SigmaReadout {
    pub surface: VolSurface,
    pub short_rv: Option<f64>,
    pub long_rv: Option<f64>,
    pub fallback: bool,
}

impl WindowsEstimator {
    pub fn new(cfg: EstimatorConfig, horizon_days: f64) -> Self {
        let har_cfg = ForecastConfig { calibration_days: cfg.calibration_days, ..ForecastConfig::default() };
        Self {
            cfg,
            samples: Vec::new(),
            last_sample_ms: i64::MIN,
            index_vol: None,
            raw: Vec::new(),
            har_cfg,
            horizon: Horizon::from_days(horizon_days),
            calibration: None,
            last_fit_ms: i64::MIN,
            har_surface: None,
            last_har_ms: i64::MIN,
            har_regime: None,
            har_sigma_mean: None,
        }
    }

    pub fn is_har(&self) -> bool {
        self.cfg.kind == "har"
    }

    pub fn set_index_vol(&mut self, annualized: Option<f64>) {
        self.index_vol = annualized.filter(|v| v.is_finite() && *v > 0.0);
    }

    pub fn push(&mut self, ts_ms: i64, price: f64) {
        let interval = self.cfg.sample_interval_s * 1000;
        if ts_ms.saturating_sub(self.last_sample_ms) < interval {
            return;
        }
        self.last_sample_ms = ts_ms;
        self.samples.push((ts_ms, price));
        // The forecaster sees the same sampled series (doc 07 §4: the
        // per-asset interval is the floor for SUI anyway); a minute-level
        // year would make every forecast re-sort a million points.
        if self.is_har() && ts_ms >= 0 {
            self.raw.push((ts_ms as u64, price));
            let keep_from = (ts_ms as u64).saturating_sub(self.har_cfg.required_history_ms() + 86_400_000);
            if self.raw.first().is_some_and(|(t, _)| *t < keep_from) {
                let first = self.raw.iter().position(|(t, _)| *t >= keep_from).unwrap_or(0);
                self.raw.drain(..first);
            }
        }
        // Keep a little more than the long window.
        let keep_from = ts_ms - (self.cfg.long_window_hours * 3_600_000.0) as i64 - interval;
        if let Some(first) = self.samples.iter().position(|(t, _)| *t >= keep_from) {
            if first > 0 {
                self.samples.drain(..first);
            }
        }
    }

    pub fn params(&self) -> SurfaceParams {
        SurfaceParams {
            risk_premium: self.cfg.risk_premium,
            skew: self.cfg.skew,
            convexity: self.cfg.convexity,
            term_short_boost: self.cfg.term_short_boost,
            term_decay_years: self.cfg.term_decay_years,
            anchor_ratio: None,
            floor_vol: self.cfg.floor_vol,
            cap_vol: self.cfg.cap_vol,
            convexity_from_kurtosis: self.cfg.convexity_from_kurtosis,
        }
    }

    /// `har`: refit on the schedule and re-forecast at the sample cadence
    /// (a forecast per minute is wasted work; the quantile moves with the
    /// realized components, which only change at the sampling interval).
    fn har_surface(&mut self, now_ms: i64) -> VolSurface {
        let params = self.params();
        let now_u = now_ms.max(0) as u64;
        let refit_ms = self.cfg.refit_secs.max(60) * 1000;
        let input = ForecastInput { asset: "asset", history: &self.raw };
        if self.calibration.is_none() || now_ms.saturating_sub(self.last_fit_ms) >= refit_ms {
            self.calibration = Some(vol_forecast::fit(&self.har_cfg, &input, self.horizon));
            self.last_fit_ms = now_ms;
            self.last_har_ms = i64::MIN;
        }
        let interval = self.cfg.sample_interval_s * 1000;
        if self.har_surface.is_none() || now_ms.saturating_sub(self.last_har_ms) >= interval {
            let cal = self.calibration.as_ref().expect("fitted above");
            let f = vol_forecast::forecast(cal, &input, now_u);
            self.har_regime = Some(format!("{:?}", f.regime).to_lowercase());
            self.har_sigma_mean = if f.is_usable() { Some(f.sigma_mean) } else { None };
            self.har_surface = Some(VolSurface::from_forecast(&f, self.cfg.q_bid, &params));
            self.last_har_ms = now_ms;
        }
        self.har_surface.expect("set above")
    }

    /// The live desk's blend (`max_lean` = 0.8) or, with `max_lean = 0`,
    /// the plain weighted mean.
    pub fn surface(&mut self, now_ms: i64) -> SigmaReadout {
        let short_rv = realized_vol(&self.samples, (self.cfg.short_window_hours * 3_600_000.0) as i64, now_ms);
        let long_rv = realized_vol(&self.samples, (self.cfg.long_window_hours * 3_600_000.0) as i64, now_ms);
        let params = self.params();
        if self.is_har() {
            let surface = self.har_surface(now_ms);
            return SigmaReadout { surface, short_rv, long_rv, fallback: surface.is_fallback() };
        }
        if self.cfg.kind == "vol_index" {
            let surface = VolSurface::from_windows(&[WindowSample { annualized_vol: self.index_vol, weight: 1.0 }], self.cfg.fallback_vol, &params);
            return SigmaReadout { surface, short_rv, long_rv, fallback: surface.is_fallback() };
        }
        let surface = if self.cfg.max_lean >= 0.8 - 1e-12 && self.cfg.max_lean <= 0.8 + 1e-12 {
            VolSurface::from_windows(
                &[
                    WindowSample { annualized_vol: short_rv, weight: self.cfg.short_window_weight },
                    WindowSample { annualized_vol: long_rv, weight: self.cfg.long_window_weight },
                ],
                self.cfg.fallback_vol,
                &params,
            )
        } else {
            // Custom lean: blend here, then hand pricing a single window so
            // the shape/premium/clamp path is identical.
            let live: Vec<(f64, f64)> = [(short_rv, self.cfg.short_window_weight), (long_rv, self.cfg.long_window_weight)]
                .into_iter()
                .filter_map(|(v, w)| v.filter(|x| x.is_finite() && *x > 0.0 && w > 0.0).map(|x| (x, w)))
                .collect();
            let blended = if live.is_empty() {
                None
            } else {
                let wsum: f64 = live.iter().map(|(_, w)| w).sum();
                let mean = live.iter().map(|(v, w)| v * w).sum::<f64>() / wsum;
                let mx = live.iter().map(|(v, _)| *v).fold(f64::MIN, f64::max);
                Some(mean.max(self.cfg.max_lean * mx))
            };
            VolSurface::from_windows(&[WindowSample { annualized_vol: blended, weight: 1.0 }], self.cfg.fallback_vol, &params)
        };
        SigmaReadout { surface, short_rv, long_rv, fallback: surface.is_fallback() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realized_vol_annualizes_by_actual_spacing() {
        // 1% alternating returns every 15 minutes for a day.
        let step = 900_000i64;
        let mut px = 100.0;
        let mut s = Vec::new();
        for i in 0..97 {
            s.push((i * step, px));
            px *= if i % 2 == 0 { 1.01 } else { 1.0 / 1.01 };
        }
        let rv = realized_vol(&s, 86_400_000, 96 * step).unwrap();
        let per_sample = (1.01f64).ln();
        let expect = per_sample * (365.0_f64 * 96.0).sqrt();
        assert!((rv - expect).abs() / expect < 0.02, "{rv} vs {expect}");
        assert!(realized_vol(&s[..3], 86_400_000, 2 * step).is_none());
    }
}
