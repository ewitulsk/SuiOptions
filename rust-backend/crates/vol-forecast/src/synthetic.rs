//! Deterministic synthetic price paths for the crate's gate tests and the
//! G6 estimator study: a daily log-vol AR(1) (GARCH-like clustering) with
//! optional compound-Poisson jumps, plus helpers to add iid microstructure
//! noise or inject one wick (the 2025-10-10 scenario). Seeded, no external
//! RNG, byte-identical across runs.

use crate::rv::MS_PER_YEAR;

/// splitmix64 with Box-Muller normals. Not for anything but simulation.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in the open interval (0, 1).
    pub fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform();
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Stochastic-vol-with-jumps path parameters.
#[derive(Clone, Copy, Debug)]
pub struct SvJumpParams {
    pub days: u32,
    pub interval_ms: u64,
    pub start_ms: u64,
    pub s0: f64,
    /// Long-run annualized continuous vol.
    pub sigma_mean: f64,
    /// Daily log-vol AR(1) persistence.
    pub phi: f64,
    /// Daily log-vol innovation std.
    pub eta: f64,
    /// Poisson jump intensity (per day); 0 = no jumps.
    pub jumps_per_day: f64,
    /// Jump magnitude scale: |jump| = jump_size · U(0.75, 1.25), random sign.
    pub jump_size: f64,
}

impl Default for SvJumpParams {
    fn default() -> Self {
        Self {
            days: 400,
            interval_ms: 300_000,
            start_ms: 1_700_000_000_000,
            s0: 1.0,
            sigma_mean: 0.87,
            phi: 0.97,
            eta: 0.08,
            jumps_per_day: 0.0,
            jump_size: 0.0,
        }
    }
}

/// A generated path with its ground truth.
#[derive(Clone, Debug)]
pub struct SyntheticPath {
    pub history: Vec<(u64, f64)>,
    /// True continuous annualized vol per day, oldest first.
    pub daily_sigma: Vec<f64>,
    pub jump_times: Vec<u64>,
}

impl SyntheticPath {
    pub fn end_ms(&self) -> u64 {
        self.history.last().map(|s| s.0).unwrap_or(0)
    }
}

pub fn sv_jump_path(seed: u64, p: &SvJumpParams) -> SyntheticPath {
    let mut rng = Rng::new(seed);
    let per_day = (86_400_000 / p.interval_ms.max(1)) as usize;
    let dt_years = p.interval_ms as f64 / MS_PER_YEAR;
    let mu = p.sigma_mean.ln();
    let mut log_sigma = mu;
    let mut lp = p.s0.ln();
    let mut history = Vec::with_capacity(p.days as usize * per_day + 1);
    let mut daily_sigma = Vec::with_capacity(p.days as usize);
    let mut jump_times = Vec::new();
    history.push((p.start_ms, p.s0));
    let mut t = p.start_ms;
    let p_jump = p.jumps_per_day / per_day as f64;
    for _ in 0..p.days {
        log_sigma = mu + p.phi * (log_sigma - mu) + p.eta * rng.normal();
        let sigma = log_sigma.exp();
        daily_sigma.push(sigma);
        let step_std = sigma * dt_years.sqrt();
        for _ in 0..per_day {
            t += p.interval_ms;
            let mut r = step_std * rng.normal();
            if p_jump > 0.0 && rng.uniform() < p_jump {
                let mag = p.jump_size * (0.75 + 0.5 * rng.uniform());
                let sign = if rng.uniform() < 0.5 { -1.0 } else { 1.0 };
                r += sign * mag;
                jump_times.push(t);
            }
            lp += r;
            history.push((t, lp.exp()));
        }
    }
    SyntheticPath {
        history,
        daily_sigma,
        jump_times,
    }
}

/// Add iid log-price noise of std `noise_std` to every sample (bid-ask
/// bounce proxy). RV at interval Δ then carries `2·noise_std²` per return
/// on top of the efficient variance, i.e. the doc 07 §4 signature shape.
pub fn add_microstructure_noise(
    history: &[(u64, f64)],
    noise_std: f64,
    seed: u64,
) -> Vec<(u64, f64)> {
    let mut rng = Rng::new(seed);
    history
        .iter()
        .map(|&(t, p)| (t, p * (noise_std * rng.normal()).exp()))
        .collect()
}

/// Multiply every price at or after `at_ms` by `exp(log_return)`: one
/// permanent wick at that timestamp.
pub fn inject_return(history: &mut [(u64, f64)], at_ms: u64, log_return: f64) {
    let f = log_return.exp();
    for s in history.iter_mut() {
        if s.0 >= at_ms {
            s.1 *= f;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rv::realized_vol_between;

    #[test]
    fn path_is_deterministic_and_has_the_requested_vol() {
        let p = SvJumpParams {
            days: 60,
            eta: 0.0,
            ..Default::default()
        };
        let a = sv_jump_path(7, &p);
        let b = sv_jump_path(7, &p);
        assert_eq!(a.history, b.history);
        assert_eq!(a.history.len(), 60 * 288 + 1);
        let rv = realized_vol_between(&a.history, a.history[0].0, a.end_ms(), 3_600_000);
        assert!((rv / 0.87 - 1.0).abs() < 0.08, "rv {rv}");
    }

    #[test]
    fn inject_return_moves_only_the_future() {
        let mut h = vec![(0u64, 1.0), (1, 1.0), (2, 1.0)];
        inject_return(&mut h, 1, -0.55);
        assert_eq!(h[0].1, 1.0);
        assert!((h[1].1 - (-0.55f64).exp()).abs() < 1e-15);
        assert_eq!(h[1].1, h[2].1);
    }
}
