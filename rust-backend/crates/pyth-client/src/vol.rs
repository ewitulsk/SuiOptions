//! Realized-volatility math, plus a fixed-time-window buffer the bot uses
//! to keep a current sigma estimate.
//!
//! Convention: log returns are computed in *natural log* space. The
//! fixed-cadence [`realized_vol`] annualizes by `sqrt(samples_per_year)`
//! (its callers — e.g. daily Benchmarks closes in `sigma.rs` — have evenly
//! spaced samples by construction). [`RollingVolBuffer`], whose sampler can
//! skip ticks while the price stream is stale, instead annualizes from the
//! actual timestamps it retains, so gaps in the cadence don't bias sigma.

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

/// Annualized standard deviation of the log-return series. Uses sample
/// stddev (N-1). Returns `0.0` if there aren't enough samples.
pub fn realized_vol(log_returns: &[f64], samples_per_year: f64) -> f64 {
    if log_returns.len() < 2 || samples_per_year <= 0.0 {
        return 0.0;
    }
    let n = log_returns.len() as f64;
    let mean: f64 = log_returns.iter().copied().sum::<f64>() / n;
    let var: f64 = log_returns
        .iter()
        .map(|r| (r - mean).powi(2))
        .sum::<f64>()
        / (n - 1.0);
    var.sqrt() * samples_per_year.sqrt()
}

/// Rolling time-windowed price buffer. Pushing a sample older than the
/// window's max age is a no-op; pushing a newer sample evicts everything
/// outside the window. The buffer is meant to live behind a mutex — it
/// doesn't synchronize internally.
#[derive(Debug)]
pub struct RollingVolBuffer {
    window: VecDeque<(u64, f64)>, // (unix_ms, price)
    max_age_ms: u64,
}

impl RollingVolBuffer {
    pub fn new(max_age_ms: u64) -> Self {
        Self {
            window: VecDeque::new(),
            max_age_ms,
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

    /// Current annualized sigma over the retained window, time-weighted:
    /// `σ² = Σ rᵢ² / (span in years)`, where the span is the elapsed time
    /// between the first and last retained samples. Using the actual
    /// timestamps (zero-mean realized variance, the standard RV estimator)
    /// keeps the estimate unbiased when the sampler skips ticks during
    /// price-stream outages. Returns `None` if fewer than 3 samples are
    /// present or the span is zero.
    pub fn current_annualized(&self) -> Option<f64> {
        if self.window.len() < 3 {
            return None;
        }
        let prices: Vec<f64> = self.window.iter().map(|(_, p)| *p).collect();
        let returns = log_returns(&prices);
        if returns.len() < 2 {
            return None;
        }
        let span_ms = self.window.back()?.0.saturating_sub(self.window.front()?.0);
        if span_ms == 0 {
            return None;
        }
        const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;
        let span_years = span_ms as f64 / MS_PER_YEAR;
        let sum_sq: f64 = returns.iter().map(|r| r * r).sum();
        Some((sum_sq / span_years).sqrt())
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
        let mut b = RollingVolBuffer::new(60_000);
        b.push(0, 100.0);
        b.push(30_000, 101.0);
        b.push(70_000, 102.0); // pushes 0 out (cutoff = 10_000)
        assert_eq!(b.len(), 2);
        // Both retained samples are inside the 60s window.
        let _ = b.current_annualized();
    }

    #[test]
    fn buffer_returns_none_with_few_samples() {
        let mut b = RollingVolBuffer::new(60_000);
        assert!(b.current_annualized().is_none());
        b.push(0, 100.0);
        b.push(1000, 101.0);
        assert!(b.current_annualized().is_none()); // need >=3
        b.push(2000, 102.0);
        assert!(b.current_annualized().is_some());
    }

    const YEAR_MS: u64 = 365 * 86_400_000;

    #[test]
    fn buffer_sigma_matches_hand_calc() {
        // Two ln(1.1) returns over exactly one year:
        // σ = sqrt(2·ln(1.1)² / 1.0) = √2 · 0.0953102 ≈ 0.134790.
        let mut b = RollingVolBuffer::new(2 * YEAR_MS);
        b.push(0, 100.0);
        b.push(YEAR_MS / 2, 110.0);
        b.push(YEAR_MS, 121.0);
        let s = b.current_annualized().unwrap();
        assert!((s - 0.134790).abs() < 1e-5, "got {s}");
    }

    #[test]
    fn buffer_sigma_is_gap_invariant_for_same_span() {
        // Same prices and total span, one buffer sampled evenly and one with
        // a huge gap (sampler skipped ticks): time-weighting must give the
        // identical sigma — the old fixed-cadence annualization did not.
        let mut even = RollingVolBuffer::new(2 * YEAR_MS);
        even.push(0, 100.0);
        even.push(YEAR_MS / 2, 110.0);
        even.push(YEAR_MS, 121.0);
        let mut gappy = RollingVolBuffer::new(2 * YEAR_MS);
        gappy.push(0, 100.0);
        gappy.push(1_000, 110.0); // then nothing for ~a year
        gappy.push(YEAR_MS, 121.0);
        let (a, b) = (
            even.current_annualized().unwrap(),
            gappy.current_annualized().unwrap(),
        );
        assert!((a - b).abs() < 1e-12, "even {a} vs gappy {b}");
    }

    #[test]
    fn buffer_sigma_scales_with_inverse_sqrt_span() {
        // Stretching the same returns over twice the span halves the
        // variance: σ₂ = σ₁/√2.
        let mut one = RollingVolBuffer::new(4 * YEAR_MS);
        let mut two = RollingVolBuffer::new(4 * YEAR_MS);
        for (ts, p) in [(0u64, 100.0), (YEAR_MS / 2, 105.0), (YEAR_MS, 99.0)] {
            one.push(ts, p);
            two.push(ts * 2, p);
        }
        let (s1, s2) = (
            one.current_annualized().unwrap(),
            two.current_annualized().unwrap(),
        );
        assert!(
            (s2 - s1 / 2f64.sqrt()).abs() < 1e-12,
            "s1 {s1}, s2 {s2}"
        );
    }
}
