//! Read model maintained from indexer events.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tracing::{debug, trace};

use shared::protocol_types::events::{ChainEvent, IndexedEvent};
use shared::protocol_types::ids::ObjectId;

use crate::bucket::Bucket;
use crate::catalog::TokenCatalog;

pub struct AppState {
    buckets: RwLock<BTreeMap<ObjectId, Bucket>>,
    pub catalog: TokenCatalog,
}

impl AppState {
    pub fn new(catalog: TokenCatalog) -> Self {
        Self {
            buckets: RwLock::new(BTreeMap::new()),
            catalog,
        }
    }

    pub fn active_buckets(&self) -> Vec<(ObjectId, Bucket)> {
        self.buckets
            .read()
            .iter()
            .filter(|(_, v)| !v.cleaned)
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }
}

impl shared::indexer_client::EventSink for AppState {
    fn ingest_event(&self, indexed: &IndexedEvent) {
        trace!(sequence = indexed.sequence, "ingesting indexer event");
        match &indexed.event {
            ChainEvent::BucketCreated(b) => {
                debug!(
                    bucket = %b.bucket_id,
                    asset_type = %b.asset_type,
                    settlement_type = %b.settlement_type,
                    strike = b.strike,
                    expiry_ms = b.expiry_ms,
                    "BucketCreated"
                );
                self.buckets.write().insert(
                    b.bucket_id,
                    Bucket {
                        asset_type: b.asset_type.clone(),
                        settlement_type: b.settlement_type.clone(),
                        strike: b.strike,
                        expiry_ms: b.expiry_ms,
                        total_written: 0,
                        exercise_cursor: 0,
                        cleaned: false,
                    },
                );
            }
            ChainEvent::WriteExecuted(w) => {
                if let Some(v) = self.buckets.write().get_mut(&w.bucket_id) {
                    v.total_written = w.range_end;
                }
            }
            ChainEvent::Exercised(e) => {
                if let Some(v) = self.buckets.write().get_mut(&e.bucket_id) {
                    v.exercise_cursor = e.cursor_after;
                }
            }
            ChainEvent::BucketCleaned(c) => {
                if let Some(v) = self.buckets.write().get_mut(&c.bucket_id) {
                    v.cleaned = true;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::indexer_client::EventSink;
    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::events::{BucketCleaned, BucketCreated};

    fn evt(seq: u64, ev: ChainEvent) -> IndexedEvent {
        IndexedEvent {
            sequence: seq,
            timestamp_ms: 0,
            event: ev,
        }
    }

    #[test]
    fn bucket_lifecycle() {
        let s = AppState::new(TokenCatalog::default());
        let id = ObjectId::new([0xaa; 32]);
        s.ingest_event(&evt(
            1,
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: id,
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1_700_000_000_000,
                strike: 60_000_000_000,
            }),
        ));
        assert_eq!(s.active_buckets().len(), 1);
        s.ingest_event(&evt(
            2,
            ChainEvent::BucketCleaned(BucketCleaned { bucket_id: id }),
        ));
        assert_eq!(s.active_buckets().len(), 0);
    }
}
