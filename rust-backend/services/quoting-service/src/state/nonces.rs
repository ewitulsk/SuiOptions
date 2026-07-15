//! Seen-nonce table.
//!
//! With balance tracking and reservations removed (collateral abstraction,
//! plan §7), the only per-quote state the service keeps is which
//! `(signer, nonce)` pairs it has already accepted — a duplicate nonce means
//! the MM signed two quotes with the same nonce, which would revert on chain,
//! so the duplicate is rejected up front. Entries expire with the quote's
//! `valid_until_ms` (the chain's own replay window) and are pruned
//! opportunistically on insert — no background eviction task.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use tracing::{debug, trace};

use protocol_types::ids::ObjectId;

#[derive(Debug, Default)]
pub struct NonceTable {
    inner: Mutex<BTreeMap<(ObjectId, u64), u64>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// `(signer, nonce)` was already seen (and hasn't expired). Quotes carry
    /// unique nonces by construction, so the right move is to reject the
    /// duplicate quote — never overwrite.
    DuplicateKey,
}

impl NonceTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `(signer, nonce)` as seen until `valid_until_ms`. Prunes every
    /// expired entry first (`now_ms >= valid_until`), so the table stays
    /// bounded by the live-quote window without a background task.
    pub fn insert(
        &self,
        signer_id: ObjectId,
        nonce: u64,
        valid_until_ms: u64,
        now_ms: u64,
    ) -> InsertOutcome {
        let mut g = self.inner.lock();
        g.retain(|_, valid_until| now_ms < *valid_until);
        let key = (signer_id, nonce);
        if g.contains_key(&key) {
            debug!(signer = %signer_id, nonce, "duplicate nonce rejected");
            return InsertOutcome::DuplicateKey;
        }
        trace!(signer = %signer_id, nonce, valid_until_ms, "recording seen nonce");
        g.insert(key, valid_until_ms);
        metrics::gauge!("quoting_seen_nonces").set(g.len() as f64);
        InsertOutcome::Inserted
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_nonce_is_rejected() {
        let t = NonceTable::new();
        let s = ObjectId::new([0x01; 32]);
        assert_eq!(t.insert(s, 1, 1_000, 0), InsertOutcome::Inserted);
        assert_eq!(t.insert(s, 1, 9_999, 0), InsertOutcome::DuplicateKey);
        // A different signer's identical nonce is unrelated.
        assert_eq!(
            t.insert(ObjectId::new([0x02; 32]), 1, 1_000, 0),
            InsertOutcome::Inserted
        );
    }

    #[test]
    fn expired_entries_are_pruned_on_insert() {
        let t = NonceTable::new();
        let s = ObjectId::new([0x01; 32]);
        t.insert(s, 1, 100, 0);
        t.insert(s, 2, 200, 0);
        // At now=150 nonce 1 has expired: pruned, so re-inserting it works
        // (the chain's own nonce table is the real replay guard).
        assert_eq!(t.insert(s, 1, 300, 150), InsertOutcome::Inserted);
        assert_eq!(t.len(), 2);
    }
}
