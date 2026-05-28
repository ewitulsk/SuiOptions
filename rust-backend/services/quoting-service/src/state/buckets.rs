//! Bucket mirror — the off-chain view of every bucket the indexer has told
//! us about.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tracing::{debug, trace};

use protocol_types::asset::AssetType;
use protocol_types::ids::ObjectId;

#[derive(Clone, Debug)]
pub struct BucketView {
    pub asset_type: AssetType,
    pub settlement_type: AssetType,
    pub strike: u128,
    pub strike_scale: u8,
    pub expiry_ms: u64,
    pub total_written: u128,
    pub exercise_cursor: u128,
    /// Admin freeze on new writes. Reflected here so we can pre-empt
    /// RFQs and quote validation on invalidated buckets without round-
    /// tripping to the chain. Toggled by Bucket{In,Re}validated events.
    pub invalidated: bool,
}

#[derive(Default)]
pub struct BucketStore {
    by_id: RwLock<BTreeMap<ObjectId, BucketView>>,
}

impl BucketStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&self, id: ObjectId, view: BucketView) {
        debug!(%id, strike = %view.strike, strike_scale = view.strike_scale, expiry_ms = view.expiry_ms, "upserting bucket");
        self.by_id.write().insert(id, view);
    }

    pub fn set_cursor(&self, id: ObjectId, exercise_cursor: u128, total_written: u128) {
        let mut g = self.by_id.write();
        if let Some(v) = g.get_mut(&id) {
            trace!(%id, exercise_cursor, total_written, "updating bucket cursor");
            v.exercise_cursor = exercise_cursor;
            v.total_written = total_written;
        }
    }

    pub fn set_total_written(&self, id: ObjectId, total_written: u128) {
        let mut g = self.by_id.write();
        if let Some(v) = g.get_mut(&id) {
            v.total_written = total_written;
        }
    }

    pub fn set_cursor_only(&self, id: ObjectId, exercise_cursor: u128) {
        let mut g = self.by_id.write();
        if let Some(v) = g.get_mut(&id) {
            v.exercise_cursor = exercise_cursor;
        }
    }

    pub fn set_invalidated(&self, id: ObjectId, invalidated: bool) {
        let mut g = self.by_id.write();
        if let Some(v) = g.get_mut(&id) {
            trace!(%id, invalidated, "updating bucket invalidated flag");
            v.invalidated = invalidated;
        }
    }

    /// True iff we know about this bucket and its `invalidated` flag is set.
    /// Returns false for unknown buckets — callers should treat
    /// `unknown bucket` separately from `invalidated`.
    pub fn is_invalidated(&self, id: &ObjectId) -> bool {
        self.by_id
            .read()
            .get(id)
            .map(|v| v.invalidated)
            .unwrap_or(false)
    }

    pub fn get(&self, id: &ObjectId) -> Option<BucketView> {
        self.by_id.read().get(id).cloned()
    }

    pub fn all(&self) -> Vec<(ObjectId, BucketView)> {
        self.by_id
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_get() {
        let s = BucketStore::new();
        let id = ObjectId::new([0x01; 32]);
        s.upsert(
            id,
            BucketView {
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                strike: 50,
                strike_scale: 0,
                expiry_ms: 100,
                total_written: 0,
                exercise_cursor: 0,
                invalidated: false,
            },
        );
        let v = s.get(&id).unwrap();
        assert_eq!(v.strike, 50);
        s.set_total_written(id, 10_000);
        s.set_cursor_only(id, 2_500);
        let v = s.get(&id).unwrap();
        assert_eq!(v.total_written, 10_000);
        assert_eq!(v.exercise_cursor, 2_500);
    }

    #[test]
    fn invalidated_flag_toggles() {
        let s = BucketStore::new();
        let id = ObjectId::new([0x02; 32]);
        s.upsert(
            id,
            BucketView {
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                strike: 1,
                strike_scale: 0,
                expiry_ms: 1,
                total_written: 0,
                exercise_cursor: 0,
                invalidated: false,
            },
        );
        assert!(!s.is_invalidated(&id));
        s.set_invalidated(id, true);
        assert!(s.is_invalidated(&id));
        s.set_invalidated(id, false);
        assert!(!s.is_invalidated(&id));
    }

    #[test]
    fn is_invalidated_returns_false_for_unknown_bucket() {
        let s = BucketStore::new();
        assert!(!s.is_invalidated(&ObjectId::new([0x99; 32])));
    }
}
