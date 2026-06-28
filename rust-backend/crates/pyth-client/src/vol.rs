//! Realized-volatility math, plus a fixed-time-window buffer the bot uses
//! to keep a current sigma estimate.
//!
//! Convention: log returns are computed in *natural log* space, and the
//! resulting standard deviation is annualized by `sqrt(samples_per_year)`.
//! For a sample every minute that's `sqrt(525_600)`; for hourly,
//! `sqrt(8_760)`. The caller is responsible for matching the sample
//! cadence to `samples_per_year`.

use std::collections::VecDeque;

/// `ln(p_{i}/p_{i-1})` for adjacent prices. Returns an empty vec if
/// there are fewer than two prices or any price is non-positive.
pub fn log_returns(prices: &[f64]) -> Vec<f64> {
    if prices.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(prices.len() - 1);
    for w in prices.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a <= 0.0 || b <= 0.0 {
            return Vec::new();
        }
        out.push((b / a).ln());
    }
    out
}

/// Which realized-vol estimator to apply to a log-return series.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VolEstimator {
    /// Sample standard deviation (N−1) of mean-subtracted log returns. The
    /// historical default. Over a short window with a trend, subtracting the
    /// poorly-estimated sample mean removes genuine volatility and biases σ
    /// **down** — fine for long windows, lossy for option pricing.
    SampleStdDev,
    /// Zero-mean estimator: variance = mean(r²), no drift subtraction. Standard
    /// for option-vol estimation, where the per-window mean return is noise.
    ZeroMean,
    /// Exponentially-weighted (RiskMetrics) zero-mean variance with decay
    /// `lambda` ∈ (0, 1): recent returns dominate, so the estimate tracks
    /// regime changes without a hard window edge. λ≈0.94 is the daily default.
    Ewma { lambda: f64 },
}

/// Annualized standard deviation of the log-return series. Uses sample
/// stddev (N-1). Returns `0.0` if there aren't enough samples.
///
/// Equivalent to [`realized_vol_with`] under [`VolEstimator::SampleStdDev`];
/// retained as the stable name existing callers use.
pub fn realized_vol(log_returns: &[f64], samples_per_year: f64) -> f64 {
    realized_vol_with(log_returns, samples_per_year, VolEstimator::SampleStdDev)
}

/// Annualized realized vol under an explicit [`VolEstimator`]. Returns `0.0`
/// when there are too few samples or `samples_per_year <= 0`.
pub fn realized_vol_with(log_returns: &[f64], samples_per_year: f64, est: VolEstimator) -> f64 {
    if log_returns.len() < 2 || samples_per_year <= 0.0 {
        return 0.0;
    }
    let n = log_returns.len() as f64;
    let var = match est {
        VolEstimator::SampleStdDev => {
            let mean: f64 = log_returns.iter().copied().sum::<f64>() / n;
            log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0)
        }
        VolEstimator::ZeroMean => log_returns.iter().map(|r| r * r).sum::<f64>() / n,
        VolEstimator::Ewma { lambda } => {
            let lambda = lambda.clamp(1e-6, 1.0 - 1e-6);
            // Seed with the first squared return, then blend forward. The EWMA
            // is a zero-mean variance: vₜ = λ·vₜ₋₁ + (1−λ)·rₜ².
            let mut var = log_returns[0] * log_returns[0];
            for r in &log_returns[1..] {
                var = lambda * var + (1.0 - lambda) * r * r;
            }
            var
        }
    };
    (var * samples_per_year).sqrt()
}

/// Rolling time-windowed price buffer. Pushing a sample older than the
/// window's max age is a no-op; pushing a newer sample evicts everything
/// outside the window. The buffer is meant to live behind a mutex — it
/// doesn't synchronize internally.
#[derive(Debug)]
pub struct RollingVolBuffer {
    window: VecDeque<(u64, f64)>, // (unix_ms, price)
    max_age_ms: u64,
    /// Samples per year corresponding to the cadence the caller pushes
    /// at. Used to annualize the stddev when reading `current_annualized`.
    samples_per_year: f64,
    /// Estimator applied in `current_annualized`.
    estimator: VolEstimator,
}

impl RollingVolBuffer {
    /// Buffer using the legacy [`VolEstimator::SampleStdDev`].
    pub fn new(max_age_ms: u64, samples_per_year: f64) -> Self {
        Self::with_estimator(max_age_ms, samples_per_year, VolEstimator::SampleStdDev)
    }

    /// Buffer with an explicit estimator — use [`VolEstimator::ZeroMean`] or
    /// `Ewma` for option-vol estimation to avoid the short-window drift bias.
    pub fn with_estimator(max_age_ms: u64, samples_per_year: f64, estimator: VolEstimator) -> Self {
        Self {
            window: VecDeque::new(),
            max_age_ms,
            samples_per_year,
            estimator,
        }
    }

    pub fn push(&mut self, ts_ms: u64, price: f64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        // Evict anything older than `ts_ms - max_age_ms`.
        let cutoff = ts_ms.saturating_sub(self.max_age_ms);
        while matches!(self.window.front(), Some(&(t, _)) if t < cutoff) {
            self.window.pop_front();
        }
        self.window.push_back((ts_ms, price));
    }

    pub fn len(&self) -> usize {
        self.window.len()
    }

    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    /// Current annualized sigma over the retained window. Returns `None`
    /// if fewer than 3 samples are present (2 log returns minimum for a
    /// stddev, plus one to avoid trivial cases).
    pub fn current_annualized(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        let prices: Vec<f64> = self.window.iter().map(|(_, p)| *p).collect();
        let returns = log_returns(&prices);
        if returns.len() < 2 {
            return None;
        }
        Some(realized_vol_with(&returns, self.samples_per_year, self.estimator))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_prices_have_zero_vol() {
        let prices = vec![100.0; 50];
        let r = log_returns(&prices);
        assert!(realized_vol(&r, 525_600.0).abs() < 1e-12);
    }

    #[test]
    fn log_returns_skip_nonpositive() {
        assert!(log_returns(&[100.0, 0.0, 100.0]).is_empty());
        assert!(log_returns(&[100.0]).is_empty());
    }

    #[test]
    fn known_series_matches_hand_calc() {
        // Returns: ln(110/100), ln(121/110), ln(100/121) = ln(1.1), ln(1.1), ln(~0.826)
        let prices = vec![100.0, 110.0, 121.0, 100.0];
        let r = log_returns(&prices);
        assert_eq!(r.len(), 3);
        // stddev of [0.0953, 0.0953, -0.1906]: mean = -0.0000, var = sum/(n-1)
        // var = ((0.0953)^2 + (0.0953)^2 + (-0.1906)^2) / 2 ≈ 0.02725
        // stddev ≈ 0.16507
        let s = realized_vol(&r, 1.0); // not annualized
        assert!((s - 0.16507).abs() < 1e-4, "got {s}");
    }

    #[test]
    fn buffer_evicts_old_samples() {
        let mut b = RollingVolBuffer::new(60_000, 525_600.0);
        b.push(0, 100.0);
        b.push(30_000, 101.0);
        b.push(70_000, 102.0); // pushes 0 out (cutoff = 10_000)
        assert_eq!(b.len(), 2);
        // Both retained samples are inside the 60s window.
        let _ = b.current_annualized();
    }

    #[test]
    fn buffer_returns_none_with_few_samples() {
        let mut b = RollingVolBuffer::new(60_000, 525_600.0);
        assert!(b.current_annualized().is_none());
        b.push(0, 100.0);
        b.push(1000, 101.0);
        assert!(b.current_annualized().is_none()); // need >=3
        b.push(2000, 102.0);
        assert!(b.current_annualized().is_some());
    }

    #[test]
    fn zero_mean_exceeds_sample_stddev_on_a_trend() {
        // A steadily drifting series: subtracting the sample mean strips the
        // trend and under-states vol; the zero-mean estimator keeps it.
        let prices: Vec<f64> = (0..30).map(|i| 100.0 * 1.01_f64.powi(i)).collect();
        let r = log_returns(&prices);
        let sd = realized_vol_with(&r, 1.0, VolEstimator::SampleStdDev);
        let zm = realized_vol_with(&r, 1.0, VolEstimator::ZeroMean);
        // Constant-growth ⇒ near-zero sample stddev but clearly positive zero-mean.
        assert!(sd < 1e-9, "sample stddev {sd} should be ~0 on a constant drift");
        assert!(zm > 1e-3, "zero-mean {zm} should capture the drift as vol");
    }

    #[test]
    fn ewma_tracks_recent_regime() {
        // Calm then violent: EWMA weights the recent violent returns more than
        // a flat zero-mean average over the whole window.
        let mut prices = vec![100.0];
        for _ in 0..20 {
            let last = *prices.last().unwrap();
            prices.push(last * 1.0005); // calm
        }
        for i in 0..20 {
            let last = *prices.last().unwrap();
            prices.push(last * if i % 2 == 0 { 1.08 } else { 1.0 / 1.08 }); // violent
        }
        let r = log_returns(&prices);
        let flat = realized_vol_with(&r, 1.0, VolEstimator::ZeroMean);
        let ewma = realized_vol_with(&r, 1.0, VolEstimator::Ewma { lambda: 0.94 });
        assert!(ewma > flat, "ewma {ewma} should weight the recent regime above flat {flat}");
    }

    #[test]
    fn buffer_uses_configured_estimator() {
        let mut b = RollingVolBuffer::with_estimator(60_000, 1.0, VolEstimator::ZeroMean);
        b.push(0, 100.0);
        b.push(1000, 101.0);
        b.push(2000, 102.0);
        b.push(3000, 103.0);
        let zm = b.current_annualized().unwrap();
        // Same samples under sample-stddev differ (drift subtracted).
        let mut b2 = RollingVolBuffer::new(60_000, 1.0);
        b2.push(0, 100.0);
        b2.push(1000, 101.0);
        b2.push(2000, 102.0);
        b2.push(3000, 103.0);
        assert!(zm > b2.current_annualized().unwrap());
    }
}
