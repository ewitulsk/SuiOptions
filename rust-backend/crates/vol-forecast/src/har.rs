//! HAR-RV-CJ (Corsi 2009; Andersen, Bollerslev & Diebold 2007): forward
//! realized variance regressed on daily / weekly / monthly components of
//! the continuous and jump variance series, fitted by OLS.
//!
//! ```text
//! ln E[C_fwd] = β0 + β1·ln C_d + β2·ln C_w + β3·ln C_m       (log-linear)
//!    E[J_fwd] = γ0 + γ1·J_d   + γ2·J_w   + γ3·J_m,  γ_i ≥ 0  (levels)
//!    σ_fc     = s · sqrt(k·exp(ln Ĉ) + Ĵ)
//! ```
//!
//! `k` re-centres the log model in levels (mean of realized over mean of
//! fitted), `s` zeroes the σ-scale bias on the training rows. Regressors
//! are winsorized at the training support (× a multiple for C) so an
//! unprecedented input — the 2025-10-10 wick — cannot extrapolate the
//! linear jump term. `J_d` is the e-folding-decayed sum of the last day's
//! jump returns, so a wick leaves the daily term within hours instead of
//! sitting in a 24-hour bucket.

use serde::{Deserialize, Serialize};

use crate::rv::{DayStats, MS_PER_DAY};

/// Floor on the continuous variance before taking logs.
pub const C_FLOOR: f64 = 1e-8;

/// Fitted HAR weights (or the fixed fallback while cold).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarWeights {
    /// Log-continuous regression: intercept, daily, weekly, monthly.
    pub beta: [f64; 4],
    /// Level re-centering multiplier `k`.
    pub level_scale: f64,
    /// Jump regression: intercept, daily, weekly, monthly (all ≥ 0).
    pub gamma: [f64; 4],
    /// Winsorization caps on the continuous regressors (levels).
    pub c_caps: [f64; 3],
    /// Winsorization caps on the jump regressors.
    pub j_caps: [f64; 3],
    /// σ-scale bias correction `s`.
    pub sigma_scale: f64,
}

impl HarWeights {
    /// Fixed fallback weights while history is too short to fit: a
    /// geometric blend of the three continuous components and the
    /// unconditional mean jump variance.
    pub fn fixed(mean_jump_variance: f64) -> Self {
        Self {
            beta: [0.0, 0.35, 0.35, 0.30],
            level_scale: 1.0,
            gamma: [mean_jump_variance.max(0.0), 0.0, 0.0, 0.0],
            c_caps: [f64::INFINITY; 3],
            j_caps: [f64::INFINITY; 3],
            sigma_scale: 1.0,
        }
    }

    /// `(continuous variance, jump variance)` forecast, annualized, before
    /// the σ-scale correction.
    pub fn predict(&self, r: &Regressors) -> (f64, f64) {
        let mut lc = self.beta[0];
        for i in 0..3 {
            lc += self.beta[i + 1] * r.c[i].min(self.c_caps[i]).max(C_FLOOR).ln();
        }
        let var_c = self.level_scale * lc.exp();
        let mut vj = self.gamma[0];
        for i in 0..3 {
            vj += self.gamma[i + 1] * r.j[i].min(self.j_caps[i]);
        }
        (var_c.max(0.0), vj.max(0.0))
    }

    /// Annualized forecast vol.
    pub fn sigma(&self, r: &Regressors) -> f64 {
        let (vc, vj) = self.predict(r);
        self.sigma_scale * (vc + vj).sqrt()
    }
}

/// Daily / weekly / monthly components at a forecast origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Regressors {
    /// Continuous variance: [daily, weekly, monthly].
    pub c: [f64; 3],
    /// Jump variance: [daily (decayed), weekly, monthly].
    pub j: [f64; 3],
}

/// Weekly and monthly component windows, in days.
pub const WEEK_DAYS: usize = 7;
pub const MONTH_DAYS: usize = 30;

fn mean_over<F: Fn(&DayStats) -> f64>(days: &[Option<DayStats>], f: F) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0usize;
    for d in days.iter().flatten() {
        sum += f(d);
        n += 1;
    }
    (n > 0).then(|| sum / n as f64)
}

/// Components at origin day `origin` (`days[origin]` is the day ending at
/// `origin_ms`; higher indices are older). `None` when the weekly window
/// has no valid day at all.
pub fn regressors(
    days: &[Option<DayStats>],
    origin: usize,
    origin_ms: u64,
    jump_decay_ms: u64,
) -> Option<Regressors> {
    if origin >= days.len() {
        return None;
    }
    let week = &days[origin..(origin + WEEK_DAYS).min(days.len())];
    let month = &days[origin..(origin + MONTH_DAYS).min(days.len())];
    let c_w = mean_over(week, |d| d.continuous)?;
    let c_m = mean_over(month, |d| d.continuous).unwrap_or(c_w);
    let j_w = mean_over(week, |d| d.jump).unwrap_or(0.0);
    let j_m = mean_over(month, |d| d.jump).unwrap_or(j_w);
    let (c_d, j_d) = match &days[origin] {
        Some(d) => {
            let mut jd = 0.0;
            for &(ts, r2) in &d.jumps {
                let age = origin_ms.saturating_sub(ts) as f64;
                let w = if jump_decay_ms > 0 {
                    (-age / jump_decay_ms as f64).exp()
                } else {
                    1.0
                };
                jd += r2 * 365.0 * w;
            }
            (d.continuous, jd)
        }
        None => (c_w, j_w),
    };
    Some(Regressors {
        c: [c_d, c_w, c_m],
        j: [j_d, j_w, j_m],
    })
}

/// One training row: components at an origin and the realized forward
/// means over the next `h` days.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub origin: usize,
    pub x: Regressors,
    pub y_c: f64,
    pub y_j: f64,
    pub y_rv: f64,
}

/// Rows in chronological order (oldest origin first). Origins run from
/// the oldest day that still has a full weekly window behind it down to
/// day `h` (so the forward target fits inside the series).
pub fn build_rows(
    days: &[Option<DayStats>],
    end_ms: u64,
    h: usize,
    jump_decay_ms: u64,
) -> Vec<Row> {
    let h = h.max(1);
    if days.len() < WEEK_DAYS + h {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let need = h.div_ceil(2);
    for origin in (h..=days.len() - WEEK_DAYS).rev() {
        let origin_ms = end_ms - origin as u64 * MS_PER_DAY;
        let Some(x) = regressors(days, origin, origin_ms, jump_decay_ms) else {
            continue;
        };
        let fwd = &days[origin - h..origin];
        let n = fwd.iter().flatten().count();
        if n < need {
            continue;
        }
        let y_c = mean_over(fwd, |d| d.continuous).unwrap_or(0.0);
        let y_j = mean_over(fwd, |d| d.jump).unwrap_or(0.0);
        let y_rv = mean_over(fwd, |d| d.rv).unwrap_or(0.0);
        rows.push(Row {
            origin,
            x,
            y_c,
            y_j,
            y_rv,
        });
    }
    rows
}

/// Ordinary least squares on 4 columns via the normal equations with a
/// tiny ridge for rank deficiency (e.g. an all-zero jump column).
#[allow(clippy::needless_range_loop)]
fn ols(x: &[[f64; 4]], y: &[f64]) -> [f64; 4] {
    let mut xtx = [[0.0f64; 4]; 4];
    let mut xty = [0.0f64; 4];
    for (row, &yi) in x.iter().zip(y) {
        for i in 0..4 {
            xty[i] += row[i] * yi;
            for j in 0..4 {
                xtx[i][j] += row[i] * row[j];
            }
        }
    }
    let trace: f64 = (0..4).map(|i| xtx[i][i]).sum();
    let ridge = 1e-9 * (trace / 4.0).max(1e-12);
    for i in 0..4 {
        xtx[i][i] += ridge;
    }
    // Gaussian elimination with partial pivoting.
    let mut a = xtx;
    let mut b = xty;
    for col in 0..4 {
        let mut piv = col;
        for r in col + 1..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        a.swap(col, piv);
        b.swap(col, piv);
        let d = a[col][col];
        if d.abs() < 1e-300 {
            continue;
        }
        for r in col + 1..4 {
            let f = a[r][col] / d;
            for c in col..4 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut out = [0.0f64; 4];
    for col in (0..4).rev() {
        let mut s = b[col];
        for c in col + 1..4 {
            s -= a[col][c] * out[c];
        }
        out[col] = if a[col][col].abs() < 1e-300 {
            0.0
        } else {
            s / a[col][col]
        };
    }
    out
}

/// Fit HAR weights on `rows` (see module docs). `c_cap_mult` widens the
/// continuous winsorization caps beyond the training maximum.
pub fn fit_weights(rows: &[Row], c_cap_mult: f64) -> HarWeights {
    let mut c_caps = [0.0f64; 3];
    let mut j_caps = [0.0f64; 3];
    for r in rows {
        for i in 0..3 {
            c_caps[i] = c_caps[i].max(r.x.c[i]);
            j_caps[i] = j_caps[i].max(r.x.j[i]);
        }
    }
    for c in c_caps.iter_mut() {
        *c *= c_cap_mult.max(1.0);
    }

    let xc: Vec<[f64; 4]> = rows
        .iter()
        .map(|r| {
            [
                1.0,
                r.x.c[0].max(C_FLOOR).ln(),
                r.x.c[1].max(C_FLOOR).ln(),
                r.x.c[2].max(C_FLOOR).ln(),
            ]
        })
        .collect();
    let yc: Vec<f64> = rows.iter().map(|r| r.y_c.max(C_FLOOR).ln()).collect();
    let beta = ols(&xc, &yc);
    let mean_fit: f64 = xc
        .iter()
        .map(|x| (0..4).map(|i| beta[i] * x[i]).sum::<f64>().exp())
        .sum::<f64>()
        / rows.len() as f64;
    let mean_y: f64 = rows.iter().map(|r| r.y_c).sum::<f64>() / rows.len() as f64;
    let level_scale = if mean_fit > 0.0 && mean_y > 0.0 {
        mean_y / mean_fit
    } else {
        1.0
    };

    let xj: Vec<[f64; 4]> = rows
        .iter()
        .map(|r| [1.0, r.x.j[0], r.x.j[1], r.x.j[2]])
        .collect();
    let yj: Vec<f64> = rows.iter().map(|r| r.y_j).collect();
    let mut gamma = ols(&xj, &yj);
    for g in gamma.iter_mut().skip(1) {
        if !g.is_finite() || *g < 0.0 {
            *g = 0.0;
        }
    }
    // Re-centre the intercept after clipping so the mean jump forecast
    // still matches the mean realized jump variance.
    let resid_mean = rows
        .iter()
        .map(|r| r.y_j - (gamma[1] * r.x.j[0] + gamma[2] * r.x.j[1] + gamma[3] * r.x.j[2]))
        .sum::<f64>()
        / rows.len() as f64;
    gamma[0] = resid_mean.max(0.0);

    let mut w = HarWeights {
        beta,
        level_scale,
        gamma,
        c_caps,
        j_caps,
        sigma_scale: 1.0,
    };
    let mean_real: f64 =
        rows.iter().map(|r| r.y_rv.max(0.0).sqrt()).sum::<f64>() / rows.len() as f64;
    let mean_fc: f64 = rows.iter().map(|r| w.sigma(&r.x)).sum::<f64>() / rows.len() as f64;
    if mean_real > 0.0 && mean_fc > 0.0 {
        w.sigma_scale = mean_real / mean_fc;
    }
    w
}

/// `ln(σ_realized / σ_forecast)` for one row under `w`.
pub fn log_residual(w: &HarWeights, row: &Row) -> f64 {
    let fc = w.sigma(&row.x);
    let real = row.y_rv.max(0.0).sqrt();
    if fc > 0.0 && real > 0.0 {
        (real / fc).ln()
    } else {
        0.0
    }
}

/// Walk-forward residuals: expanding-window refits every `fold_rows`
/// rows, each fold scored on rows whose forward targets the training set
/// could not have seen (training origins ≥ test origin + h). Unsorted.
pub fn walk_forward_residuals(
    rows: &[Row],
    h: usize,
    min_train: usize,
    fold_rows: usize,
    c_cap_mult: f64,
) -> Vec<f64> {
    let mut out = Vec::new();
    let fold_rows = fold_rows.max(1);
    let mut i = 0usize;
    while i < rows.len() {
        let first_origin = rows[i].origin;
        let train: Vec<Row> = rows[..i]
            .iter()
            .copied()
            .filter(|r| r.origin >= first_origin + h)
            .collect();
        if train.len() < min_train {
            i += 1;
            continue;
        }
        let w = fit_weights(&train, c_cap_mult);
        let end = (i + fold_rows).min(rows.len());
        for r in &rows[i..end] {
            out.push(log_residual(&w, r));
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ols_recovers_exact_linear_coefficients() {
        let truth = [0.5, 1.5, -2.0, 0.25];
        let mut x = Vec::new();
        let mut y = Vec::new();
        for i in 0..40 {
            let f = i as f64;
            let row = [1.0, (f * 0.37).sin(), (f * 0.11).cos(), f / 40.0];
            x.push(row);
            y.push((0..4).map(|k| truth[k] * row[k]).sum());
        }
        let b = ols(&x, &y);
        for k in 0..4 {
            assert!((b[k] - truth[k]).abs() < 1e-6, "{b:?}");
        }
    }

    #[test]
    fn ols_tolerates_a_zero_column() {
        let x: Vec<[f64; 4]> = (0..20).map(|i| [1.0, i as f64, 0.0, 0.0]).collect();
        let y: Vec<f64> = (0..20).map(|i| 2.0 + 3.0 * i as f64).collect();
        let b = ols(&x, &y);
        assert!(
            (b[0] - 2.0).abs() < 1e-5 && (b[1] - 3.0).abs() < 1e-6,
            "{b:?}"
        );
        assert!(b[2].abs() < 1e-6 && b[3].abs() < 1e-6);
    }

    fn day(c: f64, j: f64) -> Option<DayStats> {
        Some(DayStats {
            rv: c + j,
            bipower: c,
            jump: j,
            continuous: c,
            has_jump: j > 0.0,
            n_valid: 96,
            n_total: 96,
            ..Default::default()
        })
    }

    #[test]
    fn regressors_average_over_available_windows_and_decay_jumps() {
        let mut days: Vec<Option<DayStats>> =
            (0..40).map(|i| day(1.0 + i as f64 * 0.01, 0.0)).collect();
        days[3] = None;
        let end = 100 * MS_PER_DAY;
        // A jump 6h before the origin inside day 0.
        let mut d0 = day(1.0, 0.0).unwrap();
        d0.jumps = vec![(end - 6 * 3_600_000, 0.01)];
        d0.has_jump = true;
        d0.jump = 0.01 * 365.0;
        days[0] = Some(d0);
        let r = regressors(&days, 0, end, 6 * 3_600_000).unwrap();
        assert!((r.c[0] - 1.0).abs() < 1e-12);
        // Weekly mean skips the missing day 3.
        let cw: f64 = [0, 1, 2, 4, 5, 6]
            .iter()
            .map(|i| 1.0 + *i as f64 * 0.01)
            .sum::<f64>()
            / 6.0;
        assert!((r.c[1] - cw).abs() < 1e-12);
        assert!((r.j[0] - 0.01 * 365.0 / std::f64::consts::E).abs() < 1e-9);
        assert!((r.j[1] - 0.01 * 365.0 / 6.0).abs() < 1e-9);
        // Missing origin day falls back to the weekly mean.
        days[0] = None;
        let r = regressors(&days, 0, end, 0).unwrap();
        assert!((r.c[0] - r.c[1]).abs() < 1e-12);
        assert!(regressors(&days, 40, end, 0).is_none());
    }

    #[test]
    fn fit_is_unbiased_on_its_own_rows() {
        let days: Vec<Option<DayStats>> = (0..120)
            .map(|i| day(0.5 + 0.3 * ((i as f64) * 0.2).sin().abs(), 0.0))
            .collect();
        let rows = build_rows(&days, 200 * MS_PER_DAY, 5, 0);
        assert!(rows.len() > 60);
        let w = fit_weights(&rows, 4.0);
        let mean_resid: f64 = rows
            .iter()
            .map(|r| r.y_rv.sqrt() - w.sigma(&r.x))
            .sum::<f64>()
            / rows.len() as f64;
        assert!(mean_resid.abs() < 1e-9, "{mean_resid}");
        assert!(w.gamma.iter().all(|g| *g >= 0.0));
    }
}
