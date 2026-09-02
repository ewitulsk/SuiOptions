//! The bid's vol input. v0 reproduces the live desk's two-window blend
//! through `pricing::surface` so the G6 study can measure it; the HAR
//! forecaster (G5, `vol-forecast`) plugs in behind the same trait once
//! it lands.

use pricing::surface::{SurfaceParams, VolSurface, WindowSample};

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
}

#[derive(Clone, Copy, Debug)]
pub struct SigmaReadout {
    pub surface: VolSurface,
    pub short_rv: Option<f64>,
    pub long_rv: Option<f64>,
    pub fallback: bool,
}

impl WindowsEstimator {
    pub fn new(cfg: EstimatorConfig) -> Self {
        Self { cfg, samples: Vec::new(), last_sample_ms: i64::MIN }
    }

    pub fn push(&mut self, ts_ms: i64, price: f64) {
        let interval = self.cfg.sample_interval_s * 1000;
        if ts_ms.saturating_sub(self.last_sample_ms) < interval {
            return;
        }
        self.last_sample_ms = ts_ms;
        self.samples.push((ts_ms, price));
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
        }
    }

    /// The live desk's blend (`max_lean` = 0.8) or, with `max_lean = 0`,
    /// the plain weighted mean.
    pub fn surface(&self, now_ms: i64) -> SigmaReadout {
        let short_rv = realized_vol(&self.samples, (self.cfg.short_window_hours * 3_600_000.0) as i64, now_ms);
        let long_rv = realized_vol(&self.samples, (self.cfg.long_window_hours * 3_600_000.0) as i64, now_ms);
        let params = self.params();
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
