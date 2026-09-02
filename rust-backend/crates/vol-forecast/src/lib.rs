//! Horizon-aware realized-volatility forecaster — the IV estimator of
//! docs/mm-bot-v2/09-backtesting-gap-remediation.md §2 (SO-440, G5).
//!
//! The desk quotes into a market with no implied vol, so the bid's σ is a
//! *forecast* of realized vol over the option's life, not trailing RV.
//! This crate is the one pure function the live desk and the backtester
//! share: same history in, byte-identical [`VolForecast`] out.
//!
//! ```text
//! fit(history, horizon)          -> Calibration   (daily, cheap)
//! forecast(&cal, history, now)   -> VolForecast   (per quote, cheaper)
//! ```
//!
//! Pipeline, per §2.2:
//!
//! 1. **Sampling interval** from the asset's volatility signature (RV at
//!    1m/5m/15m/1h/4h/1d over the last 30 days): the first interval whose
//!    RV is within `signature_tolerance` of the 1-hour value. SUI lands
//!    on 15m, BTC on 1m (doc 07 §4). It is an output, never a config.
//! 2. **Daily decomposition** at that interval: realized variance split
//!    into a continuous part (threshold bipower variation) and jumps
//!    (BNS ratio test, returns beyond 5 robust sigmas).
//! 3. **HAR-RV-CJ** (see [`har`]): log-linear HAR on the continuous
//!    daily/weekly/monthly components, linear HAR on the jump components,
//!    OLS-fitted per horizon on the calibration window. The daily jump
//!    regressor e-folds with `jump_decay_ms`, so a wick leaves the
//!    forecast within hours rather than sitting in a 24h bucket.
//! 4. **Distribution**: walk-forward residuals `ln(σ_realized/σ_forecast)`
//!    are stored sorted; `quantile(q) = σ_mean · exp(Q_q)`. Cold regime
//!    uses a lognormal with `cold_residual_std`.
//! 5. **Regime**: `Cold` (no fit), `PostShock` (a detected jump within
//!    `post_shock_days` and a short-half-life EWMA RV ≥ `post_shock_ratio`
//!    × the 30-day RV), `Elevated` (ratio alone), else `Calm`.
//! 6. **Tail inputs** for the surface: daily excess kurtosis and jump
//!    intensity from the asset's own returns.
//!
//! Deterministic (no randomness, no maps, fixed-order float sums), no
//! I/O, no async, no dependency beyond serde. Inputs are provider-neutral
//! `(unix_ms, price)` samples at any cadence.

pub mod har;
pub mod history;
mod norm;
pub mod rolling;
pub mod rv;
pub mod signature;
pub mod synthetic;

use serde::{Deserialize, Serialize};

pub use har::{HarWeights, Regressors};
pub use history::PriceHistory;
pub use rolling::RollingVolBuffer;
pub use rv::{log_returns, realized_vol, realized_vol_between, DayStats, MS_PER_DAY, MS_PER_YEAR};
pub use signature::SignaturePoint;

/// Days of components the forecast needs behind its origin (the monthly
/// HAR window).
pub const REGRESSOR_DAYS: usize = har::MONTH_DAYS;

/// Estimator parameters. Defaults are the doc 09 §2.2 starting point; the
/// G6 study sweeps them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ForecastConfig {
    /// Calibration window length (days of history the fit uses).
    pub calibration_days: u32,
    /// Window over which the volatility signature is measured.
    pub signature_days: u32,
    /// Candidate sampling intervals (must divide one day).
    pub candidate_intervals_ms: Vec<u64>,
    /// Signature reference interval (doc 07 §4 uses 1 hour).
    pub reference_interval_ms: u64,
    /// Relative RV tolerance versus the reference for interval selection.
    pub signature_tolerance: f64,
    /// Interval used when the signature cannot be measured.
    pub default_interval_ms: u64,
    /// Minimum valid returns for a signature point to count.
    pub min_signature_returns: usize,
    /// Jump threshold in robust sigmas (returns beyond it are jump
    /// candidates and are truncated in the bipower products).
    pub jump_threshold_sigmas: f64,
    /// BNS ratio-test critical value.
    pub jump_z_crit: f64,
    /// A day needs at least this fraction of valid returns to count.
    pub min_day_coverage: f64,
    /// Minimum training rows before OLS replaces the fixed weights.
    pub min_train_rows: usize,
    /// Walk-forward refit cadence, in rows (days).
    pub fold_rows: usize,
    /// Minimum walk-forward residuals before they replace in-sample ones.
    pub min_residuals: usize,
    /// Lognormal residual std used for quantiles while cold.
    pub cold_residual_std: f64,
    /// E-folding time of the daily jump regressor.
    pub jump_decay_ms: u64,
    /// Half-life of the short-window EWMA RV the regime detector uses.
    pub short_halflife_ms: u64,
    /// A detected jump within this many days arms the post-shock regime.
    pub post_shock_days: usize,
    /// Short/long RV ratio at or above which (with a recent jump) the
    /// regime is `PostShock`.
    pub post_shock_ratio: f64,
    /// Short/long RV ratio at or above which the regime is `Elevated`.
    pub elevated_ratio: f64,
    /// Horizons are clamped to this many days for fitting.
    pub max_horizon_days: usize,
    /// Continuous regressors are winsorized at this multiple of the
    /// training maximum (jump regressors at the maximum itself).
    pub c_cap_mult: f64,
}

impl Default for ForecastConfig {
    fn default() -> Self {
        Self {
            calibration_days: 365,
            signature_days: 30,
            candidate_intervals_ms: vec![
                60_000, 300_000, 900_000, 3_600_000, 14_400_000, 86_400_000,
            ],
            reference_interval_ms: 3_600_000,
            signature_tolerance: 0.08,
            default_interval_ms: 300_000,
            min_signature_returns: 200,
            jump_threshold_sigmas: 5.0,
            jump_z_crit: 3.0,
            min_day_coverage: 0.5,
            min_train_rows: 30,
            fold_rows: 30,
            min_residuals: 20,
            cold_residual_std: 0.30,
            jump_decay_ms: 6 * 3_600_000,
            short_halflife_ms: 3 * 3_600_000,
            post_shock_days: 3,
            post_shock_ratio: 1.5,
            elevated_ratio: 1.5,
            max_horizon_days: 60,
            c_cap_mult: 4.0,
        }
    }
}

impl ForecastConfig {
    /// History a caller should retain so [`fit`] sees a full calibration
    /// window.
    pub fn required_history_ms(&self) -> u64 {
        (self.calibration_days as u64 + 1) * MS_PER_DAY
    }

    fn jump_params(&self) -> rv::JumpParams {
        rv::JumpParams {
            threshold_sigmas: self.jump_threshold_sigmas,
            z_crit: self.jump_z_crit,
        }
    }
}

/// Forecast horizon (the option's remaining life).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Horizon {
    pub ms: u64,
}

impl Horizon {
    pub fn from_ms(ms: u64) -> Self {
        Self { ms }
    }

    pub fn from_days(days: f64) -> Self {
        Self {
            ms: (days.max(0.0) * MS_PER_DAY as f64).round() as u64,
        }
    }

    pub fn from_years(years: f64) -> Self {
        Self {
            ms: (years.max(0.0) * MS_PER_YEAR).round() as u64,
        }
    }

    /// Whole forward days the HAR target spans, clamped to `[1, max]`.
    pub fn days(&self, max_days: usize) -> usize {
        (self.ms.div_ceil(MS_PER_DAY) as usize).clamp(1, max_days.max(1))
    }
}

/// Volatility regime label (§2.2 item 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    Calm,
    Elevated,
    PostShock,
    Cold,
}

impl std::fmt::Display for Regime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Regime::Calm => "calm",
            Regime::Elevated => "elevated",
            Regime::PostShock => "post_shock",
            Regime::Cold => "cold",
        })
    }
}

/// Forecaster input: an asset label and its `(unix_ms, price)` history at
/// any cadence, ascending by timestamp (an unsorted slice is sorted on a
/// copy).
#[derive(Clone, Copy, Debug)]
pub struct ForecastInput<'a> {
    pub asset: &'a str,
    pub history: &'a [(u64, f64)],
}

/// The fitted state [`forecast`] consumes. Serializable so a backtest can
/// persist and replay exactly the calibration the live desk used.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub asset: String,
    pub horizon_ms: u64,
    pub horizon_days: usize,
    /// Derived per-asset sampling interval.
    pub sample_interval_ms: u64,
    /// False when the signature could not be measured and the config
    /// default interval is in use.
    pub interval_derived: bool,
    pub signature: Vec<SignaturePoint>,
    pub raw_cadence_ms: u64,
    pub weights: HarWeights,
    /// True when OLS weights replaced the fixed fallback.
    pub fitted: bool,
    pub n_rows: usize,
    /// Valid days in the calibration window.
    pub n_days: usize,
    /// Sorted walk-forward log residuals `ln(σ_realized / σ_forecast)`.
    pub residuals: Vec<f64>,
    /// True when too few walk-forward residuals existed and the final
    /// fit's in-sample residuals are used instead.
    pub residuals_in_sample: bool,
    /// Excess kurtosis of daily log returns over the window.
    pub excess_kurtosis: f64,
    /// Detected jump returns per valid day.
    pub jump_intensity_per_day: f64,
    /// Mean annualized jump variance per day.
    pub mean_jump_variance: f64,
    /// Timestamp of the newest observation the fit used.
    pub fitted_at_ms: u64,
    pub config: ForecastConfig,
}

impl Calibration {
    fn cold(cfg: &ForecastConfig, asset: &str, horizon: Horizon) -> Self {
        Self {
            asset: asset.to_string(),
            horizon_ms: horizon.ms,
            horizon_days: horizon.days(cfg.max_horizon_days),
            sample_interval_ms: cfg.default_interval_ms,
            interval_derived: false,
            signature: Vec::new(),
            raw_cadence_ms: 0,
            weights: HarWeights::fixed(0.0),
            fitted: false,
            n_rows: 0,
            n_days: 0,
            residuals: Vec::new(),
            residuals_in_sample: false,
            excess_kurtosis: 0.0,
            jump_intensity_per_day: 0.0,
            mean_jump_variance: 0.0,
            fitted_at_ms: 0,
            config: cfg.clone(),
        }
    }

    /// Whether `now_ms` is at least `refit_ms` past the last fit.
    pub fn is_due(&self, now_ms: u64, refit_ms: u64) -> bool {
        now_ms.saturating_sub(self.fitted_at_ms) >= refit_ms
    }
}

/// The forecast (§2.2). `Clone + Debug + Serialize`; identical inputs
/// serialize byte-identically.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolForecast {
    /// Expected annualized realized vol over `[now, now + horizon]`.
    pub sigma_mean: f64,
    /// Continuous (diffusive) component of `sigma_mean`.
    pub sigma_continuous: f64,
    /// Jump component of `sigma_mean` (`sigma_mean² = σ_c² + σ_j²`).
    pub sigma_jump: f64,
    pub regime: Regime,
    /// The sampling interval actually used for this asset.
    pub sample_interval_ms: u64,
    /// Fraction of the regressor lookback with usable data.
    pub coverage: f64,
    /// Age of the newest observation at `now_ms`.
    pub staleness_ms: u64,
    pub horizon_ms: u64,
    /// Short-half-life EWMA trailing RV (regime input).
    pub rv_short: f64,
    /// 30-day trailing RV (regime input).
    pub rv_long: f64,
    /// Daily excess kurtosis of the asset's own returns (calibration window).
    pub excess_kurtosis: f64,
    /// Detected jump returns per day (calibration window).
    pub jump_intensity_per_day: f64,
    /// True when OLS-fitted weights produced this forecast.
    pub calibrated: bool,
    /// Sorted log residuals backing [`quantile`](Self::quantile).
    pub residuals: Vec<f64>,
    /// Lognormal residual std used when `residuals` is empty.
    pub cold_residual_std: f64,
}

impl VolForecast {
    /// `exp(Q_q(residuals)) · sigma_mean`: the vol level realized vol is
    /// expected to exceed with probability `1 − q`. Linear interpolation
    /// on the sorted residuals; lognormal with `cold_residual_std` when
    /// there is no residual distribution.
    pub fn quantile(&self, q: f64) -> f64 {
        let q = if q.is_finite() {
            q.clamp(0.001, 0.999)
        } else {
            0.5
        };
        let z = if self.residuals.is_empty() {
            self.cold_residual_std * norm::norm_cdf_inv(q)
        } else {
            let n = self.residuals.len();
            let pos = q * (n - 1) as f64;
            let lo = pos.floor() as usize;
            let frac = pos - lo as f64;
            if lo + 1 < n {
                self.residuals[lo] + frac * (self.residuals[lo + 1] - self.residuals[lo])
            } else {
                self.residuals[n - 1]
            }
        };
        self.sigma_mean * z.exp()
    }

    /// Whether the forecast carries a usable sigma at all.
    pub fn is_usable(&self) -> bool {
        self.sigma_mean.is_finite() && self.sigma_mean > 0.0
    }

    fn unusable(cal: &Calibration, staleness_ms: u64) -> Self {
        Self {
            sigma_mean: 0.0,
            sigma_continuous: 0.0,
            sigma_jump: 0.0,
            regime: Regime::Cold,
            sample_interval_ms: cal.sample_interval_ms,
            coverage: 0.0,
            staleness_ms,
            horizon_ms: cal.horizon_ms,
            rv_short: 0.0,
            rv_long: 0.0,
            excess_kurtosis: cal.excess_kurtosis,
            jump_intensity_per_day: cal.jump_intensity_per_day,
            calibrated: false,
            residuals: Vec::new(),
            cold_residual_std: cal.config.cold_residual_std,
        }
    }
}

fn sorted_history(history: &[(u64, f64)]) -> std::borrow::Cow<'_, [(u64, f64)]> {
    if history.windows(2).all(|w| w[0].0 <= w[1].0) {
        std::borrow::Cow::Borrowed(history)
    } else {
        let mut v = history.to_vec();
        v.sort_by_key(|s| s.0);
        std::borrow::Cow::Owned(v)
    }
}

/// Excess kurtosis of `x` (0 when fewer than 20 samples or degenerate).
fn excess_kurtosis(x: &[f64]) -> f64 {
    let n = x.len();
    if n < 20 {
        return 0.0;
    }
    let mean = x.iter().sum::<f64>() / n as f64;
    let m2 = x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    let m4 = x.iter().map(|v| (v - mean).powi(4)).sum::<f64>() / n as f64;
    if m2 <= 0.0 {
        return 0.0;
    }
    m4 / (m2 * m2) - 3.0
}

/// Fit a [`Calibration`] for `horizon` on `input.history`. Cheap enough to
/// run daily; never per quote.
pub fn fit(cfg: &ForecastConfig, input: &ForecastInput<'_>, horizon: Horizon) -> Calibration {
    let hist = sorted_history(input.history);
    let mut cal = Calibration::cold(cfg, input.asset, horizon);
    if hist.len() < 2 {
        return cal;
    }
    let end = hist[hist.len() - 1].0;
    let first = hist[0].0;
    cal.fitted_at_ms = end;

    // 1. Sampling interval from the signature.
    let sig_span = cfg.signature_days as u64 * MS_PER_DAY;
    let sig_start = hist.partition_point(|s| s.0 < end.saturating_sub(sig_span));
    cal.raw_cadence_ms = signature::raw_cadence_ms(&hist[sig_start..]);
    cal.signature =
        signature::volatility_signature(&hist, end, sig_span, &cfg.candidate_intervals_ms);
    match signature::derive_interval(
        &cal.signature,
        cal.raw_cadence_ms,
        cfg.reference_interval_ms,
        cfg.signature_tolerance,
        cfg.min_signature_returns,
    ) {
        Some(interval) => {
            cal.sample_interval_ms = interval;
            cal.interval_derived = true;
        }
        None => {
            cal.sample_interval_ms = cfg.default_interval_ms;
            cal.interval_derived = false;
        }
    }
    let interval = cal.sample_interval_ms;

    // 2. Daily decomposition over the calibration window.
    let span_days = ((end - first) / MS_PER_DAY + 1)
        .min(cfg.calibration_days as u64)
        .max(1);
    let grid = rv::resample(&hist, end, span_days * MS_PER_DAY, interval);
    let days = rv::daily_series(&grid, cfg.min_day_coverage, &cfg.jump_params());
    let valid: Vec<&DayStats> = days.iter().flatten().collect();
    cal.n_days = valid.len();
    if !valid.is_empty() {
        cal.mean_jump_variance = valid.iter().map(|d| d.jump).sum::<f64>() / valid.len() as f64;
        cal.jump_intensity_per_day =
            valid.iter().map(|d| d.jumps.len() as f64).sum::<f64>() / valid.len() as f64;
    }
    let per_day = (MS_PER_DAY / interval) as usize;
    let mut daily_returns = Vec::new();
    if per_day > 0 && grid.len() > per_day {
        let mut k = grid.len() - 1;
        loop {
            let prev = k - per_day;
            if grid.covered[k] && grid.covered[prev] {
                daily_returns.push(grid.log_prices[k] - grid.log_prices[prev]);
            }
            if prev < per_day {
                break;
            }
            k = prev;
        }
    }
    cal.excess_kurtosis = excess_kurtosis(&daily_returns);
    cal.weights = HarWeights::fixed(cal.mean_jump_variance);

    // 3. HAR fit + walk-forward residuals.
    let h = cal.horizon_days;
    let rows = har::build_rows(&days, end, h, cfg.jump_decay_ms);
    cal.n_rows = rows.len();
    if rows.len() >= cfg.min_train_rows {
        cal.weights = har::fit_weights(&rows, cfg.c_cap_mult);
        cal.fitted = true;
        let mut resid = har::walk_forward_residuals(
            &rows,
            h,
            cfg.min_train_rows,
            cfg.fold_rows,
            cfg.c_cap_mult,
        );
        if resid.len() < cfg.min_residuals {
            resid = rows
                .iter()
                .map(|r| har::log_residual(&cal.weights, r))
                .collect();
            cal.residuals_in_sample = true;
        }
        resid.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        cal.residuals = resid;
    }
    cal
}

/// Forecast realized vol over `[now_ms, now_ms + horizon]` from
/// `input.history` under `cal`. Pure and deterministic.
pub fn forecast(cal: &Calibration, input: &ForecastInput<'_>, now_ms: u64) -> VolForecast {
    let cfg = &cal.config;
    let hist = sorted_history(input.history);
    let Some(&(end, _)) = hist.last() else {
        return VolForecast::unusable(cal, 0);
    };
    let staleness_ms = now_ms.saturating_sub(end);
    let interval = cal.sample_interval_ms.max(1);
    let start = hist.partition_point(|s| {
        s.0 < end.saturating_sub(REGRESSOR_DAYS as u64 * MS_PER_DAY + interval)
    });
    let window = &hist[start..];
    let grid = rv::resample(window, end, REGRESSOR_DAYS as u64 * MS_PER_DAY, interval);
    let coverage = grid.coverage();
    let days = rv::daily_series(&grid, cfg.min_day_coverage, &cfg.jump_params());

    let Some(reg) = har::regressors(&days, 0, end, cfg.jump_decay_ms) else {
        // No valid day at all: fall back to whatever returns exist.
        let (r, valid) = grid.returns();
        let kept: Vec<f64> = r
            .iter()
            .zip(&valid)
            .filter(|(_, v)| **v)
            .map(|(r, _)| *r)
            .collect();
        let sigma = realized_vol(&kept, interval);
        let mut out = VolForecast::unusable(cal, staleness_ms);
        out.coverage = coverage;
        if sigma.is_finite() && sigma > 0.0 {
            out.sigma_mean = sigma;
            out.sigma_continuous = sigma;
            out.rv_short = sigma;
            out.rv_long = sigma;
        }
        return out;
    };

    let (var_c, var_j) = cal.weights.predict(&reg);
    let s = cal.weights.sigma_scale;
    let sigma_mean = s * (var_c + var_j).sqrt();
    let sigma_continuous = s * var_c.sqrt();
    let sigma_jump = s * var_j.sqrt();

    // Regime inputs: short EWMA RV vs 30-day RV, recent detected jump.
    let (r, valid) = grid.returns();
    let lambda = if cfg.short_halflife_ms > 0 {
        (-(std::f64::consts::LN_2) * interval as f64 / cfg.short_halflife_ms as f64).exp()
    } else {
        0.0
    };
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..r.len() {
        num *= lambda;
        den *= lambda;
        if valid[i] {
            num += r[i] * r[i];
            den += 1.0;
        }
    }
    let rv_short = if den > 0.0 {
        (num / den / (interval as f64 / MS_PER_YEAR)).sqrt()
    } else {
        0.0
    };
    let valid_days: Vec<&DayStats> = days.iter().flatten().collect();
    let rv_long = if valid_days.is_empty() {
        0.0
    } else {
        (valid_days.iter().map(|d| d.rv).sum::<f64>() / valid_days.len() as f64).sqrt()
    };
    let recent_jump = days
        .iter()
        .take(cfg.post_shock_days)
        .flatten()
        .any(|d| d.has_jump);
    let ratio = if rv_long > 0.0 {
        rv_short / rv_long
    } else {
        0.0
    };
    let regime = if !cal.fitted {
        Regime::Cold
    } else if recent_jump && ratio >= cfg.post_shock_ratio {
        Regime::PostShock
    } else if ratio >= cfg.elevated_ratio {
        Regime::Elevated
    } else {
        Regime::Calm
    };

    VolForecast {
        sigma_mean,
        sigma_continuous,
        sigma_jump,
        regime,
        sample_interval_ms: interval,
        coverage,
        staleness_ms,
        horizon_ms: cal.horizon_ms,
        rv_short,
        rv_long,
        excess_kurtosis: cal.excess_kurtosis,
        jump_intensity_per_day: cal.jump_intensity_per_day,
        calibrated: cal.fitted,
        residuals: cal.residuals.clone(),
        cold_residual_std: cfg.cold_residual_std,
    }
}

/// Convenience: fit and forecast in one call (backtest sweeps, tests).
pub fn fit_and_forecast(
    cfg: &ForecastConfig,
    input: &ForecastInput<'_>,
    horizon: Horizon,
    now_ms: u64,
) -> (Calibration, VolForecast) {
    let cal = fit(cfg, input, horizon);
    let fc = forecast(&cal, input, now_ms);
    (cal, fc)
}

#[cfg(test)]
mod tests;
