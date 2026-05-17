//! Sui checkpoint Worker.
//!
//! Implements [`sui_data_ingestion_core::Worker`]. The framework hands us
//! `CheckpointData` instances in order; we walk every transaction's emitted
//! events, filter to the type strings we recognize (see
//! [`crate::event_types`]), BCS-deserialize into the `shared::protocol_types::events`
//! mirror structs, and ingest into the [`Store`] — which is what fans the
//! result out to the quoting service over WS.
//!
//! Pattern mirrors Pismo's `PositionEventWorker`. Differences:
//!
//! - Single dispatch fn (`event_types::dispatch`) instead of an if-chain in
//!   the Worker body, which keeps the per-event match pure and unit-testable
//!   without spinning up a Worker.
//! - No DB writes (Postgres deferred); the in-memory `Store` is the only
//!   persistence right now.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::{debug, error, info};

use sui_data_ingestion_core::Worker;
use sui_types::full_checkpoint_content::CheckpointData;

use crate::event_types::{self, EventTypes};
use crate::store::Store;

pub struct ProtocolEventWorker {
    store: Arc<Store>,
    types: EventTypes,
}

impl ProtocolEventWorker {
    pub fn new(store: Arc<Store>, package_id: &str) -> Self {
        let types = EventTypes::for_package(package_id);
        info!(package_id, "indexer worker listening for events");
        for t in types.all_strings() {
            debug!(event_type = t, "subscribed");
        }
        Self { store, types }
    }
}

#[async_trait]
impl Worker for ProtocolEventWorker {
    type Result = ();

    async fn process_checkpoint(&self, checkpoint: &CheckpointData) -> Result<()> {
        let seq = checkpoint.checkpoint_summary.sequence_number;
        let ts_ms = checkpoint.checkpoint_summary.timestamp_ms;
        let mut ingested = 0usize;

        for tx in &checkpoint.transactions {
            let Some(events) = &tx.events else { continue };
            let tx_digest = tx.transaction.digest().base58_encode();
            for event in &events.data {
                let type_str = event.type_.to_string();
                match event_types::dispatch(&self.types, &type_str, &event.contents) {
                    Ok(Some(parsed)) => {
                        self.store.ingest(parsed, ts_ms);
                        ingested += 1;
                    }
                    Ok(None) => {
                        // Not one of our events. Sui hands us every event in
                        // the checkpoint; we filter here rather than at the
                        // framework level so a future event type added by the
                        // contracts can be added with a single dispatch arm.
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            tx = %tx_digest,
                            event_type = %type_str,
                            "BCS decode of known event type failed — schema drift?"
                        );
                    }
                }
            }
        }

        if ingested > 0 {
            debug!(checkpoint = seq, ingested, "checkpoint processed");
        }
        let _ = (ts_ms,); // keep ts_ms in scope for future use (e.g. lag metric)
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::events::BucketCreated;
    use shared::protocol_types::ids::ObjectId;

    /// Sanity-check that an event the worker would receive via Sui's framework
    /// (BCS bytes + matching type string) round-trips through the dispatch
    /// fn into our in-memory store with all fields intact. The Worker trait
    /// itself is awkward to exercise without a real checkpoint, so we test
    /// the pieces it calls.
    #[test]
    fn dispatch_into_store_round_trips_a_bucket_created_event() {
        let pkg = "0xabc";
        let store = Arc::new(Store::new(8));
        let worker = ProtocolEventWorker::new(Arc::clone(&store), pkg);

        let evt = BucketCreated {
            bucket_id: ObjectId::new([0x99; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            expiry_ms: 1_700_000_000_000,
            strike: 50_000_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        let chain_event = event_types::dispatch(&worker.types, &worker.types.bucket_created, &bytes)
            .unwrap()
            .unwrap();
        worker.store.ingest(chain_event, 12345);

        let bucket = store.bucket(&ObjectId::new([0x99; 32])).unwrap();
        assert_eq!(bucket.strike, 50_000_000_000);
        assert_eq!(bucket.asset_type.as_str(), "BTC");
        assert_eq!(bucket.settlement_type.as_str(), "USDC");
    }
}
