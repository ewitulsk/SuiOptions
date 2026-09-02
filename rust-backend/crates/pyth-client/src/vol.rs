//! Realized-volatility math for evenly spaced samples.
//!
//! Convention: log returns are computed in *natural log* space. The
//! fixed-cadence [`realized_vol`] annualizes by `sqrt(samples_per_year)`
//! (its callers — e.g. daily Benchmarks closes in `sigma.rs` — have evenly
//! spaced samples by construction). The desk's time-windowed buffer
//! (`vol_forecast::RollingVolBuffer`, SO-450) instead annualizes from the
//! actual timestamps it retains, so gaps in the cadence don't bias sigma.

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
}
