//! Synthetic market paths for fixtures and regression tests.

use crate::data::Bar;

/// A deterministic LCG random walk at ~45% annualized vol plus a slow
/// sine, 1-minute bars from `start_ms`. The v0 test path: the zero-latency
/// regression in `engine::tests` is pinned to it.
pub fn synthetic_bars(days: i64, start_ms: i64) -> Vec<Bar> {
    let mut out = Vec::new();
    let n = days * 1440;
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut px = 3.0f64;
    for i in 0..n {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u = ((state >> 11) as f64) / ((1u64 << 53) as f64); // [0,1)
        let r = (u - 0.5) * 2.0 * 0.0006 * 1.732; // uniform with sd ≈ 0.0006 per minute
        let t = i as f64 / 1440.0;
        px *= (r + 0.0001 * (t * 0.7).cos() / 1440.0).exp();
        out.push(Bar { ts_ms: start_ms + i * 60_000, open: px, high: px, low: px, close: px, volume: 1.0 });
    }
    out
}
