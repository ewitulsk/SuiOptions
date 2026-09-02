//! Realized-variance building blocks: log returns, previous-tick
//! resampling onto a regular grid, and per-day realized / bipower /
//! jump decomposition (Barndorff-Nielsen & Shephard 2004, with the
//! Corsi-Pirino-Renò threshold correction so one wick cannot inflate the
//! continuous estimate through the adjacent product).
//!
//! Units: every variance here is **annualized** (365-day year) unless the
//! name says otherwise; log returns are natural-log.

/// Milliseconds in one calendar day.
pub const MS_PER_DAY: u64 = 86_400_000;
/// Milliseconds in a 365-day year (crypto trades every day).
pub const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;

/// `ln(p_i / p_{i-1})` for adjacent prices. Empty if fewer than two prices
/// or any price is non-positive.
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

/// Annualized zero-mean realized vol of fixed-cadence log returns:
/// `sqrt(Σ r² / span_years)`. `0.0` when there is nothing to measure.
pub fn realized_vol(returns: &[f64], interval_ms: u64) -> f64 {
    if returns.is_empty() || interval_ms == 0 {
        return 0.0;
    }
    let sum_sq: f64 = returns.iter().map(|r| r * r).sum();
    let span_years = returns.len() as f64 * interval_ms as f64 / MS_PER_YEAR;
    (sum_sq / span_years).sqrt()
}

/// A regular previous-tick grid of log prices ending at `end_ms`.
#[derive(Clone, Debug)]
pub struct Grid {
    pub interval_ms: u64,
    pub end_ms: u64,
    /// Log price at each grid point, oldest first; NaN before the first
    /// raw observation.
    pub log_prices: Vec<f64>,
    /// Whether a raw observation fell inside `(t_k − interval, t_k]`, i.e.
    /// the grid point is fresh rather than a carried-forward stale tick.
    pub covered: Vec<bool>,
}

impl Grid {
    pub fn len(&self) -> usize {
        self.log_prices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log_prices.is_empty()
    }

    /// Timestamp of grid point `k` (0 = oldest).
    pub fn time_at(&self, k: usize) -> u64 {
        let n = self.log_prices.len() as u64;
        self.end_ms
            .saturating_sub((n.saturating_sub(1).saturating_sub(k as u64)) * self.interval_ms)
    }

    /// Grid log returns (`len() − 1` of them) and whether each is valid,
    /// i.e. both of its endpoints are covered and finite. Returns spanning
    /// a data gap are dropped rather than attributed to one bucket; the
    /// per-day statistics rescale by coverage.
    pub fn returns(&self) -> (Vec<f64>, Vec<bool>) {
        let n = self.log_prices.len();
        if n < 2 {
            return (Vec::new(), Vec::new());
        }
        let mut r = Vec::with_capacity(n - 1);
        let mut valid = Vec::with_capacity(n - 1);
        for k in 1..n {
            let a = self.log_prices[k - 1];
            let b = self.log_prices[k];
            let ok = self.covered[k - 1] && self.covered[k] && a.is_finite() && b.is_finite();
            r.push(if ok { b - a } else { 0.0 });
            valid.push(ok);
        }
        (r, valid)
    }

    /// Fraction of grid points that are covered.
    pub fn coverage(&self) -> f64 {
        if self.covered.is_empty() {
            return 0.0;
        }
        self.covered.iter().filter(|c| **c).count() as f64 / self.covered.len() as f64
    }
}

/// Previous-tick resample of `history` (sorted ascending by timestamp,
/// prices positive) onto `span_ms / interval_ms + 1` points ending at
/// `end_ms`. Linear in `history.len() + grid points`.
pub fn resample(history: &[(u64, f64)], end_ms: u64, span_ms: u64, interval_ms: u64) -> Grid {
    let interval_ms = interval_ms.max(1);
    // Never reach before t = 0: shorten the grid instead of shifting it.
    let n = ((span_ms.min(end_ms) / interval_ms) as usize) + 1;
    let mut log_prices = Vec::with_capacity(n);
    let mut covered = Vec::with_capacity(n);
    let start = end_ms - (n as u64 - 1) * interval_ms;
    let mut i = 0usize; // count of raw observations with ts <= t
    for k in 0..n {
        let t = start + k as u64 * interval_ms;
        while i < history.len() && history[i].0 <= t {
            i += 1;
        }
        if i == 0 {
            log_prices.push(f64::NAN);
            covered.push(false);
            continue;
        }
        let (ts, p) = history[i - 1];
        let lp = if p > 0.0 && p.is_finite() {
            p.ln()
        } else {
            f64::NAN
        };
        log_prices.push(lp);
        covered.push(lp.is_finite() && ts + interval_ms > t);
    }
    Grid {
        interval_ms,
        end_ms,
        log_prices,
        covered,
    }
}

/// Annualized (jump-inclusive) realized vol of `history` over
/// `[start_ms, end_ms]` sampled previous-tick at `interval_ms`; the plain
/// trailing-window estimator the forecaster is compared against.
pub fn realized_vol_between(
    history: &[(u64, f64)],
    start_ms: u64,
    end_ms: u64,
    interval_ms: u64,
) -> f64 {
    let grid = resample(
        history,
        end_ms,
        end_ms.saturating_sub(start_ms),
        interval_ms,
    );
    let (r, valid) = grid.returns();
    let kept: Vec<f64> = r
        .iter()
        .zip(valid.iter())
        .filter(|(_, v)| **v)
        .map(|(r, _)| *r)
        .collect();
    realized_vol(&kept, interval_ms)
}

/// Jump-detection parameters (see [`day_stats`]).
#[derive(Clone, Copy, Debug)]
pub struct JumpParams {
    /// Returns beyond this many robust (MAD) sigmas are candidate jumps
    /// and are truncated inside the bipower / tripower products.
    pub threshold_sigmas: f64,
    /// BNS ratio-statistic critical value; a day's jumps count only when
    /// the test rejects "no jump" at this level.
    pub z_crit: f64,
}

/// One day's realized-variance decomposition, all annualized.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DayStats {
    /// Total realized variance `Σ r²`.
    pub rv: f64,
    /// Threshold bipower variance (continuous-part estimator).
    pub bipower: f64,
    /// Jump variance: `Σ r²` over the flagged jump returns when the BNS
    /// test is significant, else 0.
    pub jump: f64,
    /// `rv − jump`.
    pub continuous: f64,
    /// BNS ratio-jump z statistic (0 when undefined).
    pub z_stat: f64,
    pub n_valid: usize,
    pub n_total: usize,
    pub has_jump: bool,
    /// `(timestamp_ms, r²)` of each flagged jump return (empty unless
    /// `has_jump`). Raw, not annualized.
    pub jumps: Vec<(u64, f64)>,
}

/// Decompose one day's grid returns. `returns`, `valid` and `times` are
/// the day's slices (oldest first; `times[i]` is the end of return `i`).
/// `None` when fewer than `min_coverage` of the returns are valid.
pub fn day_stats(
    returns: &[f64],
    valid: &[bool],
    times: &[u64],
    interval_ms: u64,
    min_coverage: f64,
    jp: &JumpParams,
) -> Option<DayStats> {
    let n_total = returns.len();
    if n_total == 0 {
        return None;
    }
    let mut v: Vec<f64> = Vec::with_capacity(n_total);
    let mut t: Vec<u64> = Vec::with_capacity(n_total);
    for i in 0..n_total {
        if valid[i] {
            v.push(returns[i]);
            t.push(times[i]);
        }
    }
    let n = v.len();
    if n < 2 || (n as f64) < min_coverage * n_total as f64 {
        return None;
    }
    let ann = MS_PER_YEAR / (n as f64 * interval_ms as f64);

    // Robust scale: 1.4826 · median|r| (≈ σ for a zero-mean normal).
    let mut abs: Vec<f64> = v.iter().map(|r| r.abs()).collect();
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = if n % 2 == 1 {
        abs[n / 2]
    } else {
        0.5 * (abs[n / 2 - 1] + abs[n / 2])
    };
    let scale = 1.4826 * median;
    let threshold = if scale > 0.0 && jp.threshold_sigmas > 0.0 {
        jp.threshold_sigmas * scale
    } else {
        f64::INFINITY
    };

    let rv_raw: f64 = v.iter().map(|r| r * r).sum();

    // Threshold bipower variation: (π/2) · Σ |r_i||r_{i−1}| with each
    // |r| capped at the threshold, small-sample factor n/(n−1).
    let tr = |x: f64| x.abs().min(threshold);
    let mut bp = 0.0;
    for i in 1..n {
        bp += tr(v[i]) * tr(v[i - 1]);
    }
    let bipower_raw = (std::f64::consts::FRAC_PI_2) * bp * n as f64 / (n as f64 - 1.0);

    // BNS ratio statistic with (threshold) tripower quarticity.
    let z_stat = if n >= 3 && rv_raw > 0.0 && bipower_raw > 0.0 {
        const MU43: f64 = 0.830_872_9; // 2^{2/3} Γ(7/6) / Γ(1/2)
        let mut tp = 0.0;
        for i in 2..n {
            tp += tr(v[i]).powf(4.0 / 3.0)
                * tr(v[i - 1]).powf(4.0 / 3.0)
                * tr(v[i - 2]).powf(4.0 / 3.0);
        }
        let tq = n as f64 * tp / (MU43 * MU43 * MU43) * n as f64 / (n as f64 - 2.0);
        let theta = std::f64::consts::PI * std::f64::consts::PI / 4.0 + std::f64::consts::PI - 5.0;
        let denom = (theta / n as f64 * (tq / (bipower_raw * bipower_raw)).max(1.0)).sqrt();
        if denom > 0.0 {
            (1.0 - bipower_raw / rv_raw) / denom
        } else {
            0.0
        }
    } else {
        0.0
    };

    let mut jumps = Vec::new();
    let mut jump_raw = 0.0;
    if z_stat > jp.z_crit && threshold.is_finite() {
        for i in 0..n {
            if v[i].abs() > threshold {
                jumps.push((t[i], v[i] * v[i]));
                jump_raw += v[i] * v[i];
            }
        }
    }
    let has_jump = !jumps.is_empty();
    if !has_jump {
        jump_raw = 0.0;
    }
    Some(DayStats {
        rv: rv_raw * ann,
        bipower: bipower_raw * ann,
        jump: jump_raw * ann,
        continuous: (rv_raw - jump_raw) * ann,
        z_stat,
        n_valid: n,
        n_total,
        has_jump,
        jumps,
    })
}

/// Per-day decomposition of a grid whose span is a whole number of days.
/// Index 0 is the most recent day (ending at `grid.end_ms`); `None` marks
/// a day with insufficient coverage.
pub fn daily_series(grid: &Grid, min_coverage: f64, jp: &JumpParams) -> Vec<Option<DayStats>> {
    let per_day = (MS_PER_DAY / grid.interval_ms.max(1)) as usize;
    if per_day == 0 || grid.len() < 2 {
        return Vec::new();
    }
    let (returns, valid) = grid.returns();
    let n_ret = returns.len();
    let n_days = n_ret / per_day;
    let mut out = Vec::with_capacity(n_days);
    for d in 0..n_days {
        let hi = n_ret - d * per_day; // exclusive
        let lo = hi - per_day;
        let times: Vec<u64> = (lo..hi).map(|k| grid.time_at(k + 1)).collect();
        out.push(day_stats(
            &returns[lo..hi],
            &valid[lo..hi],
            &times,
            grid.interval_ms,
            min_coverage,
            jp,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_returns_match_hand_calc() {
        let r = log_returns(&[100.0, 110.0, 121.0, 100.0]);
        assert_eq!(r.len(), 3);
        assert!((r[0] - 1.1f64.ln()).abs() < 1e-12);
        assert!(log_returns(&[100.0, 0.0, 100.0]).is_empty());
        assert!(log_returns(&[100.0]).is_empty());
    }

    #[test]
    fn realized_vol_annualizes_by_span() {
        // Two ln(1.1) returns over one year: sqrt(2·ln(1.1)²).
        let r = [1.1f64.ln(), 1.1f64.ln()];
        let s = realized_vol(&r, (MS_PER_YEAR / 2.0) as u64);
        assert!((s - 0.134790).abs() < 1e-5, "got {s}");
        assert_eq!(realized_vol(&[], 1), 0.0);
    }

    #[test]
    fn resample_is_previous_tick_and_tracks_coverage() {
        let h = [(10_000u64, 100.0), (11_000, 101.0), (15_000, 102.0)];
        let g = resample(&h, 16_000, 6_000, 1_000);
        assert_eq!(g.len(), 7);
        assert!((g.log_prices[0] - 100f64.ln()).abs() < 1e-12);
        assert!(g.covered[0] && g.covered[1]);
        assert!(!g.covered[2]); // 2_000: carried 101 from t=1_000
        assert!(g.covered[5]); // 5_000: fresh
        assert!(!g.covered[6]); // 6_000: carried
        let (r, v) = g.returns();
        assert_eq!(r.len(), 6);
        assert!(v[0]);
        assert!(!v[1]);
        // Grid before the first observation is NaN / uncovered.
        let g = resample(&h, 16_000, 10_000, 1_000);
        assert!(g.log_prices[0].is_nan() && !g.covered[0]);
        assert_eq!(g.time_at(0), 6_000);
        // A span reaching before t = 0 shortens the grid, never shifts it.
        let g = resample(&h, 16_000, 100_000, 1_000);
        assert_eq!(g.len(), 17);
        assert_eq!(g.time_at(0), 0);
        assert_eq!(g.time_at(16), 16_000);
    }

    fn jp() -> JumpParams {
        JumpParams {
            threshold_sigmas: 5.0,
            z_crit: 3.0,
        }
    }

    #[test]
    fn day_stats_no_jump_on_gaussian_returns() {
        // Bipower variation estimates the same integrated variance as RV
        // for diffusive (Gaussian) returns; no jump is flagged.
        let mut rng = crate::synthetic::Rng::new(3);
        let n = 288;
        let r: Vec<f64> = (0..n).map(|_| 0.003 * rng.normal()).collect();
        let valid = vec![true; n];
        let times: Vec<u64> = (0..n as u64).map(|i| (i + 1) * 300_000).collect();
        let d = day_stats(&r, &valid, &times, 300_000, 0.5, &jp()).unwrap();
        assert!(!d.has_jump, "z={}", d.z_stat);
        assert!((d.continuous - d.rv).abs() < 1e-12);
        let sum_sq: f64 = r.iter().map(|x| x * x).sum();
        assert!((d.rv - sum_sq * 365.0).abs() < 1e-9, "{}", d.rv);
        assert!(
            (d.bipower / d.rv - 1.0).abs() < 0.15,
            "bv/rv {}",
            d.bipower / d.rv
        );
    }

    #[test]
    fn day_stats_isolates_a_single_wick() {
        let n = 96;
        let mut r: Vec<f64> = (0..n)
            .map(|i| if i % 3 == 0 { 0.01 } else { -0.005 })
            .collect();
        r[40] = -0.55;
        let valid = vec![true; n];
        let times: Vec<u64> = (0..n as u64).map(|i| (i + 1) * 900_000).collect();
        let d = day_stats(&r, &valid, &times, 900_000, 0.5, &jp()).unwrap();
        assert!(d.has_jump, "z={}", d.z_stat);
        assert_eq!(d.jumps.len(), 1);
        assert_eq!(d.jumps[0].0, 41 * 900_000);
        // The continuous part is what the other 95 returns say, not the wick.
        let base: f64 = r
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 40)
            .map(|(_, x)| x * x)
            .sum();
        assert!((d.continuous - base * 365.0).abs() < 1e-9);
        assert!(d.jump > 50.0 * d.continuous);
    }

    #[test]
    fn day_stats_rejects_thin_coverage() {
        let r = vec![0.01; 10];
        let mut valid = vec![false; 10];
        valid[0] = true;
        valid[1] = true;
        let times: Vec<u64> = (0..10).map(|i| (i + 1) * 1_000).collect();
        assert!(day_stats(&r, &valid, &times, 1_000, 0.5, &jp()).is_none());
    }
}
