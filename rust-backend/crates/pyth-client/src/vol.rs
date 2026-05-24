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
    /// Samples per year corresponding to the cadence the caller pushes
    /// at. Used to annualize the stddev when reading `current_annualized`.
    samples_per_year: f64,
}

impl RollingVolBuffer {
    pub fn new(max_age_ms: u64, samples_per_year: f64) -> Self {
        Self {
            window: VecDeque::new(),
            max_age_ms,
            samples_per_year,
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
        Some(realized_vol(&returns, self.samples_per_year))
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
}
