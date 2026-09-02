//! Time-bounded price history buffer the forecaster reads from. Keeps
//! `(unix_ms, price)` samples sorted ascending in a contiguous `Vec` so
//! [`forecast`](crate::forecast) can borrow them as a slice without
//! copying. Meant to live behind a lock; no internal synchronization.

/// Rolling price history with lazy front eviction.
#[derive(Clone, Debug)]
pub struct PriceHistory {
    samples: Vec<(u64, f64)>,
    max_age_ms: u64,
}

impl PriceHistory {
    pub fn new(max_age_ms: u64) -> Self {
        Self {
            samples: Vec::new(),
            max_age_ms,
        }
    }

    /// Append a sample. Non-finite / non-positive prices and out-of-order
    /// timestamps (older than the newest retained sample) are dropped so
    /// the buffer stays a valid forecaster input.
    pub fn push(&mut self, ts_ms: u64, price: f64) {
        if !price.is_finite() || price <= 0.0 {
            return;
        }
        if let Some(&(last, _)) = self.samples.last() {
            if ts_ms < last {
                return;
            }
        }
        self.samples.push((ts_ms, price));
        let cutoff = ts_ms.saturating_sub(self.max_age_ms);
        let stale = self.samples.partition_point(|&(t, _)| t < cutoff);
        // Amortize the drain: only compact once the stale prefix is a
        // meaningful fraction of the buffer.
        if stale > 0 && (stale >= 4096 || stale * 8 >= self.samples.len()) {
            self.samples.drain(..stale);
        }
    }

    /// Retained samples, oldest first. May include a small stale prefix
    /// (older than `max_age_ms`) between compactions; the forecaster only
    /// reads the window it needs.
    pub fn samples(&self) -> &[(u64, f64)] {
        &self.samples
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Timestamp of the newest sample.
    pub fn last_ts(&self) -> Option<u64> {
        self.samples.last().map(|s| s.0)
    }

    pub fn max_age_ms(&self) -> u64 {
        self.max_age_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_sorted_and_evicts_stale_prefix() {
        let mut h = PriceHistory::new(1_000);
        h.push(0, 1.0);
        h.push(100, 1.0);
        h.push(50, 1.0); // out of order: dropped
        h.push(200, f64::NAN); // dropped
        assert_eq!(h.len(), 2);
        // Stale prefix is compacted once it is ≥ 1/8 of the buffer.
        h.push(2_000, 1.0);
        assert_eq!(h.samples()[0].0, 2_000);
        assert_eq!(h.last_ts(), Some(2_000));
    }
}
