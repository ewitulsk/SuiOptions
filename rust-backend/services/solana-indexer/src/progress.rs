//! Ingestion progress, surfaced by `GET /progress`.
//!
//! Written by the worker (per confirmed / finalized slot notification) and
//! read by the HTTP layer. Lock-light so the hot path never blocks on a
//! reader. Unlike the Sui indexer there's no separate chain-tip poller —
//! a live LaserStream subscription IS the tip, so staleness is reported as
//! `ms_since_last_slot` instead of a percentage toward a polled head.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

/// Half-life of the throughput EWMA — the reported rate reflects roughly
/// the last half-minute of ingestion.
const RATE_HALF_LIFE_SECS: f64 = 30.0;

pub struct ProgressState {
    /// Slot this run started from — persisted progress, pinned config, or
    /// the first slot the stream delivered.
    start_slot: AtomicU64,
    /// Highest confirmed slot durably persisted.
    current: AtomicU64,
    /// Reorg watermark: highest finalized slot notification seen.
    finalized: AtomicU64,
    /// Millis-since-epoch of the last confirmed slot notification; drives
    /// the staleness field and the stall alert.
    last_slot_wall_ms: AtomicI64,
    rate: Mutex<RateEwma>,
}

impl ProgressState {
    pub fn new(start_slot: u64, current: u64, finalized: u64) -> Self {
        Self {
            start_slot: AtomicU64::new(start_slot),
            current: AtomicU64::new(current),
            finalized: AtomicU64::new(finalized),
            last_slot_wall_ms: AtomicI64::new(0),
            rate: Mutex::new(RateEwma::new()),
        }
    }

    /// Record a persisted confirmed slot. Monotonic — out-of-order samples
    /// are dropped.
    pub fn record_slot(&self, slot: u64) {
        // First delivery pins the bar's origin when nothing was persisted.
        let _ = self
            .start_slot
            .compare_exchange(0, slot, Ordering::Relaxed, Ordering::Relaxed);
        self.current.fetch_max(slot, Ordering::Relaxed);
        self.last_slot_wall_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
        if let Ok(mut rate) = self.rate.lock() {
            rate.observe(slot, Instant::now());
        }
    }

    pub fn record_finalized(&self, slot: u64) {
        self.finalized.fetch_max(slot, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let last_wall = self.last_slot_wall_ms.load(Ordering::Relaxed);
        let ms_since_last_slot =
            (last_wall != 0).then(|| (chrono::Utc::now().timestamp_millis() - last_wall).max(0));
        ProgressSnapshot {
            start_slot: self.start_slot.load(Ordering::Relaxed),
            current_slot: self.current.load(Ordering::Relaxed),
            finalized_slot: self.finalized.load(Ordering::Relaxed),
            rate_slots_per_sec: self.rate.lock().map(|r| r.rate()).unwrap_or(0.0),
            ms_since_last_slot,
        }
    }
}

/// JSON body of `GET /progress`.
#[derive(Serialize, Clone, Debug)]
pub struct ProgressSnapshot {
    pub start_slot: u64,
    pub current_slot: u64,
    pub finalized_slot: u64,
    pub rate_slots_per_sec: f64,
    /// `null` until the first slot lands. Solana confirms ~2-3 slots/sec,
    /// so anything beyond a few seconds means the stream is stalled.
    pub ms_since_last_slot: Option<i64>,
}

/// Time-weighted EWMA of slots/sec (same shape as the Sui indexer's).
struct RateEwma {
    last: Option<(u64, Instant)>,
    ewma: Option<f64>,
}

impl RateEwma {
    fn new() -> Self {
        Self {
            last: None,
            ewma: None,
        }
    }

    fn observe(&mut self, slot: u64, now: Instant) {
        if let Some((last_slot, last_t)) = self.last {
            let dt = now.duration_since(last_t).as_secs_f64();
            if dt > 0.0 && slot > last_slot {
                let instant_rate = (slot - last_slot) as f64 / dt;
                let alpha = 1.0 - (0.5_f64).powf(dt / RATE_HALF_LIFE_SECS);
                self.ewma = Some(match self.ewma {
                    Some(prev) => prev + alpha * (instant_rate - prev),
                    None => instant_rate,
                });
            }
        }
        self.last = Some((slot, now));
    }

    fn rate(&self) -> f64 {
        self.ewma.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_monotonic_current_and_finalized() {
        let p = ProgressState::new(100, 100, 90);
        p.record_slot(105);
        p.record_slot(103); // out-of-order: ignored
        p.record_finalized(95);
        p.record_finalized(94); // out-of-order: ignored
        let s = p.snapshot();
        assert_eq!(s.current_slot, 105);
        assert_eq!(s.finalized_slot, 95);
        assert_eq!(s.start_slot, 100);
        assert!(s.ms_since_last_slot.is_some());
    }

    #[test]
    fn first_slot_pins_start_when_unset() {
        let p = ProgressState::new(0, 0, 0);
        p.record_slot(500);
        assert_eq!(p.snapshot().start_slot, 500);
    }
}
