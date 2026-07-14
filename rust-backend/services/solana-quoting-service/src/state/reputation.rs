//! MM reputation — unchanged from the Sui twin except the account key is a
//! base58 pubkey string.
//!
//! Counters move forward only — every observed quote-related event
//! increments one bucket. Derived rates are read on demand.
//!
//! In MVP we observe but do not act on these stats; the gating logic lives
//! in the RFQ layer, behind a feature switch decided per deployment.

use dashmap::DashMap;
use parking_lot::Mutex;

#[derive(Clone, Copy, Debug, Default)]
pub struct ReputationStats {
    pub quotes_signed: u64,
    pub quotes_executed: u64,
    pub quotes_expired_unused: u64,
    pub quotes_reverted: u64,
}

impl ReputationStats {
    pub fn fill_rate(&self) -> f64 {
        if self.quotes_signed == 0 {
            return 1.0;
        }
        self.quotes_executed as f64 / self.quotes_signed as f64
    }

    pub fn revert_rate(&self) -> f64 {
        if self.quotes_signed == 0 {
            return 0.0;
        }
        self.quotes_reverted as f64 / self.quotes_signed as f64
    }

    pub fn composite_score(&self) -> f64 {
        // Reputation ∈ [0, 1]. Penalize reverts heavily, reward fills.
        let r = self.revert_rate();
        let f = self.fill_rate();
        (f - 2.0 * r).clamp(0.0, 1.0)
    }
}

#[derive(Default)]
pub struct ReputationStore {
    by_account: DashMap<String, Mutex<ReputationStats>>,
}

impl ReputationStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_mut<R>(&self, id: &str, f: impl FnOnce(&mut ReputationStats) -> R) -> R {
        let entry = self
            .by_account
            .entry(id.to_string())
            .or_insert_with(|| Mutex::new(ReputationStats::default()));
        let mut g = entry.lock();
        f(&mut *g)
    }

    pub fn record_signed(&self, id: &str) {
        self.with_mut(id, |s| s.quotes_signed += 1);
    }

    pub fn record_executed(&self, id: &str) {
        self.with_mut(id, |s| s.quotes_executed += 1);
    }

    pub fn record_expired_unused(&self, id: &str) {
        self.with_mut(id, |s| s.quotes_expired_unused += 1);
    }

    pub fn record_reverted(&self, id: &str) {
        self.with_mut(id, |s| s.quotes_reverted += 1);
    }

    pub fn snapshot(&self, id: &str) -> ReputationStats {
        self.by_account
            .get(id)
            .map(|e| *e.lock())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_starts_optimistic() {
        let s = ReputationStats::default();
        assert!((s.composite_score() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn revert_dominates_fill() {
        // 10 signed, 9 executed, 1 reverted → 90% fill, 10% revert.
        // composite = 0.9 - 2*0.1 = 0.7
        let s = ReputationStats {
            quotes_signed: 10,
            quotes_executed: 9,
            quotes_expired_unused: 0,
            quotes_reverted: 1,
        };
        assert!((s.composite_score() - 0.7).abs() < 1e-9);
    }

    #[test]
    fn store_records_per_account() {
        let store = ReputationStore::new();
        store.record_signed("a");
        store.record_signed("a");
        store.record_executed("a");
        store.record_signed("b");
        store.record_reverted("b");
        assert_eq!(store.snapshot("a").quotes_signed, 2);
        assert_eq!(store.snapshot("a").quotes_executed, 1);
        assert_eq!(store.snapshot("b").quotes_signed, 1);
        assert_eq!(store.snapshot("b").quotes_reverted, 1);
    }
}
