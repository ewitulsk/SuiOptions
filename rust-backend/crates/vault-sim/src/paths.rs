//! Price paths driving the simulation: historical replay plus the three
//! synthetic generators from doc 06 §8. All generators are seeded
//! (`rand_chacha`) for reproducibility; the backtester records seeds in its
//! reports.

use rand::Rng;
use rand_chacha::rand_core::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// One bar (daily in the standard scenarios).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub ts_ms: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl Bar {
    /// A close-only bar; OHLC collapse to the close. Synthetic generators
    /// produce these — the engine and policies only read closes.
    pub fn flat(ts_ms: u64, close: f64) -> Self {
        Self { ts_ms, open: close, high: close, low: close, close }
    }
}

/// One simulated price history.
#[derive(Clone, Debug)]
pub struct Path {
    pub bars: Vec<Bar>,
    /// Per-bar annualized vol, when the generator knows it (GARCH). Lets
    /// the IV proxy be path-consistent instead of borrowing real history.
    pub sigma_ann: Option<Vec<f64>>,
    /// Seed that produced this path (0 for replay).
    pub seed: u64,
}

/// Doc 06 §4: replay yields one path; Monte Carlo sources yield `n_paths`.
pub trait PathSource {
    fn next_path(&mut self) -> Option<Path>;
}

/// Historical replay: yields the loaded bars once.
pub struct Replay {
    bars: Option<Vec<Bar>>,
}

impl Replay {
    pub fn new(bars: Vec<Bar>) -> Self {
        Self { bars: Some(bars) }
    }
}

impl PathSource for Replay {
    fn next_path(&mut self) -> Option<Path> {
        self.bars.take().map(|bars| Path { bars, sigma_ann: None, seed: 0 })
    }
}

const DAY_MS: u64 = 86_400_000;

fn bars_from_log_returns(start_price: f64, returns: &[f64]) -> Vec<Bar> {
    let mut bars = Vec::with_capacity(returns.len() + 1);
    let mut price = start_price;
    bars.push(Bar::flat(0, price));
    for (i, r) in returns.iter().enumerate() {
        price *= r.exp();
        bars.push(Bar::flat((i as u64 + 1) * DAY_MS, price));
    }
    bars
}

fn log_returns(bars: &[Bar]) -> Vec<f64> {
    bars.windows(2).map(|w| (w[1].close / w[0].close).ln()).collect()
}

/// Stationary block bootstrap (Politis–Romano) over historical daily log
/// returns: geometric block lengths avoid seam artifacts while preserving
/// fat tails and vol clustering. The headline generator for SUI (doc 06
/// §8).
pub struct StationaryBootstrap {
    returns: Vec<f64>,
    start_price: f64,
    /// Mean block length in bars (doc suggests 10 days).
    pub mean_block: f64,
    pub horizon_bars: usize,
    n_remaining: usize,
    rng: ChaCha8Rng,
    next_seed: u64,
}

impl StationaryBootstrap {
    pub fn new(
        history: &[Bar],
        mean_block: f64,
        horizon_bars: usize,
        n_paths: usize,
        seed: u64,
    ) -> Self {
        assert!(history.len() >= 30, "need at least 30 bars of history");
        assert!(mean_block >= 1.0);
        Self {
            returns: log_returns(history),
            start_price: history.last().unwrap().close,
            mean_block,
            horizon_bars,
            n_remaining: n_paths,
            rng: ChaCha8Rng::seed_from_u64(seed),
            next_seed: seed,
        }
    }
}

impl PathSource for StationaryBootstrap {
    fn next_path(&mut self) -> Option<Path> {
        if self.n_remaining == 0 {
            return None;
        }
        self.n_remaining -= 1;
        let seed = self.next_seed;
        self.next_seed += 1;

        let n = self.returns.len();
        let p_new_block = 1.0 / self.mean_block;
        let mut out = Vec::with_capacity(self.horizon_bars);
        let mut idx = self.rng.gen_range(0..n);
        while out.len() < self.horizon_bars {
            out.push(self.returns[idx]);
            if self.rng.gen_bool(p_new_block) {
                idx = self.rng.gen_range(0..n); // new block start
            } else {
                idx = (idx + 1) % n; // continue the block, wrap at the end
            }
        }
        Some(Path { bars: bars_from_log_returns(self.start_price, &out), sigma_ann: None, seed })
    }
}

/// GBM with Merton jumps — parametric cross-check (doc 06 §8). All
/// parameters annualized; bars are daily.
pub struct GbmJump {
    pub start_price: f64,
    pub mu: f64,
    pub sigma: f64,
    /// Expected jumps per year.
    pub jump_intensity: f64,
    /// Mean / stddev of jump log-size.
    pub jump_mean: f64,
    pub jump_std: f64,
    pub horizon_bars: usize,
    n_remaining: usize,
    rng: ChaCha8Rng,
    next_seed: u64,
}

impl GbmJump {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        start_price: f64,
        mu: f64,
        sigma: f64,
        jump_intensity: f64,
        jump_mean: f64,
        jump_std: f64,
        horizon_bars: usize,
        n_paths: usize,
        seed: u64,
    ) -> Self {
        Self {
            start_price,
            mu,
            sigma,
            jump_intensity,
            jump_mean,
            jump_std,
            horizon_bars,
            n_remaining: n_paths,
            rng: ChaCha8Rng::seed_from_u64(seed),
            next_seed: seed,
        }
    }
}

fn normal(rng: &mut ChaCha8Rng) -> f64 {
    // Box–Muller; no extra dependency for one distribution.
    let u1: f64 = rng.gen_range(f64::EPSILON..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

impl PathSource for GbmJump {
    fn next_path(&mut self) -> Option<Path> {
        if self.n_remaining == 0 {
            return None;
        }
        self.n_remaining -= 1;
        let seed = self.next_seed;
        self.next_seed += 1;

        let dt = 1.0 / 365.0;
        let drift = (self.mu - 0.5 * self.sigma * self.sigma) * dt;
        let p_jump = (self.jump_intensity * dt).min(1.0);
        let returns: Vec<f64> = (0..self.horizon_bars)
            .map(|_| {
                let mut r = drift + self.sigma * dt.sqrt() * normal(&mut self.rng);
                if self.rng.gen_bool(p_jump) {
                    r += self.jump_mean + self.jump_std * normal(&mut self.rng);
                }
                r
            })
            .collect();
        Some(Path {
            bars: bars_from_log_returns(self.start_price, &returns),
            sigma_ann: None,
            seed,
        })
    }
}

/// GARCH(1,1) — vol-clustering cross-check. Emits the per-bar annualized
/// vol series so IV proxies can be path-consistent (doc 06 §8).
pub struct Garch11 {
    pub start_price: f64,
    pub mu_daily: f64,
    /// Daily-variance parameters: var_t = omega + alpha·r²_{t−1} +
    /// beta·var_{t−1}.
    pub omega: f64,
    pub alpha: f64,
    pub beta: f64,
    pub horizon_bars: usize,
    n_remaining: usize,
    rng: ChaCha8Rng,
    next_seed: u64,
}

impl Garch11 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        start_price: f64,
        mu_daily: f64,
        omega: f64,
        alpha: f64,
        beta: f64,
        horizon_bars: usize,
        n_paths: usize,
        seed: u64,
    ) -> Self {
        assert!(alpha + beta < 1.0, "GARCH must be stationary");
        Self {
            start_price,
            mu_daily,
            omega,
            alpha,
            beta,
            horizon_bars,
            n_remaining: n_paths,
            rng: ChaCha8Rng::seed_from_u64(seed),
            next_seed: seed,
        }
    }
}

impl PathSource for Garch11 {
    fn next_path(&mut self) -> Option<Path> {
        if self.n_remaining == 0 {
            return None;
        }
        self.n_remaining -= 1;
        let seed = self.next_seed;
        self.next_seed += 1;

        let mut var = self.omega / (1.0 - self.alpha - self.beta); // long-run
        let mut returns = Vec::with_capacity(self.horizon_bars);
        let mut sigma_ann = Vec::with_capacity(self.horizon_bars + 1);
        sigma_ann.push((var * 365.0).sqrt());
        for _ in 0..self.horizon_bars {
            let r = self.mu_daily + var.sqrt() * normal(&mut self.rng);
            returns.push(r);
            var = self.omega + self.alpha * r * r + self.beta * var;
            sigma_ann.push((var * 365.0).sqrt());
        }
        Some(Path {
            bars: bars_from_log_returns(self.start_price, &returns),
            sigma_ann: Some(sigma_ann),
            seed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(n: usize) -> Vec<Bar> {
        // Deterministic wobble around 100.
        (0..n)
            .map(|i| Bar::flat(i as u64 * DAY_MS, 100.0 + (i as f64 * 0.7).sin() * 5.0))
            .collect()
    }

    #[test]
    fn replay_yields_once() {
        let mut src = Replay::new(history(40));
        let p = src.next_path().unwrap();
        assert_eq!(p.bars.len(), 40);
        assert!(src.next_path().is_none());
    }

    #[test]
    fn bootstrap_is_seeded_and_reproducible() {
        let h = history(120);
        let mut a = StationaryBootstrap::new(&h, 10.0, 200, 2, 42);
        let mut b = StationaryBootstrap::new(&h, 10.0, 200, 2, 42);
        let pa = a.next_path().unwrap();
        let pb = b.next_path().unwrap();
        assert_eq!(pa.bars.len(), 201); // start bar + horizon returns
        assert_eq!(
            pa.bars.iter().map(|x| x.close).collect::<Vec<_>>(),
            pb.bars.iter().map(|x| x.close).collect::<Vec<_>>()
        );
        // Different seeds → different paths.
        let mut c = StationaryBootstrap::new(&h, 10.0, 200, 1, 43);
        let pc = c.next_path().unwrap();
        assert_ne!(
            pa.bars.iter().map(|x| x.close).collect::<Vec<_>>(),
            pc.bars.iter().map(|x| x.close).collect::<Vec<_>>()
        );
        // Yields exactly n_paths.
        assert!(a.next_path().is_some());
        assert!(a.next_path().is_none());
    }

    #[test]
    fn bootstrap_resamples_only_historical_returns() {
        let h = history(60);
        let hist_returns: Vec<f64> = log_returns(&h);
        let mut src = StationaryBootstrap::new(&h, 5.0, 100, 1, 7);
        let p = src.next_path().unwrap();
        for w in p.bars.windows(2) {
            let r = (w[1].close / w[0].close).ln();
            assert!(
                hist_returns.iter().any(|hr| (hr - r).abs() < 1e-12),
                "return {r} not from history"
            );
        }
    }

    #[test]
    fn gbm_jump_has_roughly_target_vol() {
        let mut src = GbmJump::new(100.0, 0.0, 0.6, 0.0, 0.0, 0.0, 365 * 20, 1, 1);
        let p = src.next_path().unwrap();
        let rets = log_returns(&p.bars);
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var =
            rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        let ann = (var * 365.0).sqrt();
        assert!((ann - 0.6).abs() < 0.05, "annualized vol {ann}");
    }

    #[test]
    fn garch_emits_path_consistent_sigma() {
        // omega chosen for ~60% long-run annualized vol:
        // var_daily = 0.6²/365 ≈ 9.86e-4; omega = var·(1−α−β).
        let var_daily = 0.36 / 365.0;
        let mut src =
            Garch11::new(100.0, 0.0, var_daily * 0.1, 0.1, 0.8, 365 * 4, 1, 5);
        let p = src.next_path().unwrap();
        let sig = p.sigma_ann.unwrap();
        assert_eq!(sig.len(), p.bars.len());
        // Long-run sigma is the anchor.
        assert!((sig[0] - 0.6).abs() < 1e-9);
        let mean_sig = sig.iter().sum::<f64>() / sig.len() as f64;
        assert!((mean_sig - 0.6).abs() < 0.15, "mean sigma {mean_sig}");
    }
}
