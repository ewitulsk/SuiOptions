//! Checkpoint-ingestion progress, surfaced by `GET /progress` (SO-107).
//!
//! Written from two places — the worker (one `record_checkpoint` per
//! persisted checkpoint) and a background tip-poller (`set_tip`) — and read by
//! the HTTP handler. Every field is lock-light so the hot ingestion path never
//! blocks on a reader.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

/// Half-life of the throughput EWMA. ~30s means the reported rate reflects the
/// last half-minute of ingestion — responsive to slowdowns without being jumpy.
const RATE_HALF_LIFE_SECS: f64 = 30.0;

/// Shared progress view. Construct once at boot, wrap in an `Arc`, and hand
/// clones to the worker, the tip-poller, and the HTTP layer.
pub struct ProgressState {
    /// Checkpoint this index run started from — the loading bar's 0%. The
    /// config-pinned `start_checkpoint`, or the boot tip when tailing live.
    start_checkpoint: u64,
    /// Highest checkpoint durably persisted so far.
    current: AtomicU64,
    /// Latest checkpoint available on-chain — the bar's 100%. `0` until the
    /// tip-poller's first successful query (surfaced to clients as `null`).
    tip: AtomicU64,
    rate: Mutex<RateEwma>,
}

impl ProgressState {
    pub fn new(start_checkpoint: u64, current: u64) -> Self {
        Self {
            start_checkpoint,
            current: AtomicU64::new(current),
            tip: AtomicU64::new(0),
            rate: Mutex::new(RateEwma::new()),
        }
    }

    /// Record a freshly-persisted checkpoint. Called by the worker after the
    /// Postgres commit. Safe to call out of order under ingestion concurrency —
    /// `current` never regresses and out-of-order samples are dropped.
    pub fn record_checkpoint(&self, checkpoint: u64) {
        self.current.fetch_max(checkpoint, Ordering::Relaxed);
        if let Ok(mut rate) = self.rate.lock() {
            rate.observe(checkpoint, Instant::now());
        }
    }

    /// Update the cached chain tip. Called by the background poller.
    pub fn set_tip(&self, tip: u64) {
        self.tip.store(tip, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let current = self.current.load(Ordering::Relaxed);
        let tip = self.tip.load(Ordering::Relaxed);
        let rate = self.rate.lock().map(|r| r.rate()).unwrap_or(0.0);
        ProgressSnapshot {
            start_checkpoint: self.start_checkpoint,
            current_checkpoint: current,
            tip_checkpoint: (tip != 0).then_some(tip),
            rate_checkpoints_per_sec: rate,
            caught_up: tip != 0 && current >= tip,
        }
    }
}

/// JSON body of `GET /progress`. The frontend derives the percentage and ETA
/// from these fields.
#[derive(Serialize, Clone, Debug)]
pub struct ProgressSnapshot {
    pub start_checkpoint: u64,
    pub current_checkpoint: u64,
    /// `null` until the chain tip has been polled at least once.
    pub tip_checkpoint: Option<u64>,
    pub rate_checkpoints_per_sec: f64,
    pub caught_up: bool,
}

/// Time-weighted EWMA of checkpoints/sec. The sampling interval varies (fast
/// during backfill, ~1/s when tailing), so the smoothing factor is derived from
/// the elapsed time between samples rather than a fixed alpha.
struct RateEwma {
    last: Option<(u64, Instant)>,
    ewma: Option<f64>,
}

impl RateEwma {
    fn new() -> Self {
        Self { last: None, ewma: None }
    }

    fn observe(&mut self, checkpoint: u64, now: Instant) {
        if let Some((last_cp, last_t)) = self.last {
            let dt = now.duration_since(last_t).as_secs_f64();
            if dt > 0.0 && checkpoint > last_cp {
                let instant_rate = (checkpoint - last_cp) as f64 / dt;
                let alpha = 1.0 - (0.5_f64).powf(dt / RATE_HALF_LIFE_SECS);
                self.ewma = Some(match self.ewma {
                    Some(prev) => prev + alpha * (instant_rate - prev),
                    None => instant_rate,
                });
            }
        }
        self.last = Some((checkpoint, now));
    }

    fn rate(&self) -> f64 {
        self.ewma.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_start_current_and_unknown_tip() {
        let p = ProgressState::new(100, 100);
        let s = p.snapshot();
        assert_eq!(s.start_checkpoint, 100);
        assert_eq!(s.current_checkpoint, 100);
        assert_eq!(s.tip_checkpoint, None);
        assert!(!s.caught_up);
        assert_eq!(s.rate_checkpoints_per_sec, 0.0);
    }

    #[test]
    fn current_never_regresses_and_tip_drives_caught_up() {
        let p = ProgressState::new(100, 100);
        p.record_checkpoint(105);
        p.record_checkpoint(103); // out-of-order: ignored
        p.set_tip(105);
        let s = p.snapshot();
        assert_eq!(s.current_checkpoint, 105);
        assert_eq!(s.tip_checkpoint, Some(105));
        assert!(s.caught_up);
    }

    #[test]
    fn ewma_seeds_to_first_instant_rate() {
        let mut r = RateEwma::new();
        let t0 = Instant::now();
        r.observe(0, t0);
        // 10 checkpoints in 2s → 5 cp/s for the first (seeding) sample.
        r.observe(10, t0 + std::time::Duration::from_secs(2));
        assert!((r.rate() - 5.0).abs() < 1e-9);
    }
}
