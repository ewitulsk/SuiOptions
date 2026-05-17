//! Sui checkpoint Worker.
//!
//! Implements [`sui_data_ingestion_core::Worker`]. The framework hands us
//! `CheckpointData` instances in order; we walk every transaction's emitted
//! events, filter to the type strings we recognize (see
//! [`crate::event_types`]), BCS-deserialize into the `shared::protocol_types::events`
//! mirror structs, and persist them.
//!
//! Per checkpoint:
//!   1. Decode + collect every recognised event in checkpoint order.
//!   2. [`Store::stage_batch`] under a single lock: applies materialised-view
//!      mutations, assigns monotonic sequences, builds a [`CheckpointBatch`].
//!   3. [`Repo::apply_checkpoint`] writes the batch in one transaction.
//!   4. Only after Postgres commits do we broadcast — guaranteeing the
//!      fanout never emits something that isn't durably stored.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, error, info};

use sui_data_ingestion_core::Worker;
use sui_types::full_checkpoint_content::CheckpointData;

use crate::db::Repo;
use crate::event_types::{self, EventTypes};
use crate::store::Store;

pub struct ProtocolEventWorker {
    store: Arc<Store>,
    repo: Repo,
    types: EventTypes,
}

impl ProtocolEventWorker {
    pub fn new(store: Arc<Store>, repo: Repo, package_id: &str) -> Self {
        let types = EventTypes::for_package(package_id);
        info!(package_id, "indexer worker listening for events");
        for t in types.all_strings() {
            debug!(event_type = t, "subscribed");
        }
        Self { store, repo, types }
    }
}

#[async_trait]
impl Worker for ProtocolEventWorker {
    type Result = ();

    async fn process_checkpoint(&self, checkpoint: &CheckpointData) -> Result<()> {
        let seq = checkpoint.checkpoint_summary.sequence_number;
        let ts_ms = checkpoint.checkpoint_summary.timestamp_ms;

        // Decode pass — collect everything we recognise in checkpoint order
        // before touching the store. Keeps the lock window in step 2 small.
        let mut decoded: Vec<(shared::protocol_types::events::ChainEvent, String, i32)> =
            Vec::new();
        for tx in &checkpoint.transactions {
            let Some(events) = &tx.events else { continue };
            let tx_digest = tx.transaction.digest().base58_encode();
            for (idx, event) in events.data.iter().enumerate() {
                let type_str = event.type_.to_string();
                match event_types::dispatch(&self.types, &type_str, &event.contents) {
                    Ok(Some(parsed)) => {
                        decoded.push((parsed, tx_digest.clone(), idx as i32));
                    }
                    Ok(None) => {}
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

        // Stage → persist → broadcast. If the DB write fails, return Err so
        // the framework retries; on a hard crash, boot-time hydration from
        // `indexer_progress` corrects any in-memory drift.
        let staged = self.store.stage_batch(seq, ts_ms, decoded)?;
        self.repo
            .apply_checkpoint(&staged.db_batch)
            .with_context(|| format!("persisting checkpoint {seq}"))?;
        self.store.broadcast_staged(&staged.indexed);

        if !staged.indexed.is_empty() {
            debug!(
                checkpoint = seq,
                ingested = staged.indexed.len(),
                "checkpoint persisted"
            );
        }
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
    /// fn into our in-memory store with all fields intact. The full Worker
    /// path additionally writes to Postgres — covered in integration tests
    /// that spin up a real DB; this one stays unit-level by exercising the
    /// dispatch + Store::ingest fast path directly.
    #[test]
    fn dispatch_round_trips_a_bucket_created_event_into_store() {
        let pkg = "0xabc";
        let types = EventTypes::for_package(pkg);
        let store = Store::new(8);

        let evt = BucketCreated {
            bucket_id: ObjectId::new([0x99; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            expiry_ms: 1_700_000_000_000,
            strike: 50_000_000_000,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        let chain_event = event_types::dispatch(&types, &types.bucket_created, &bytes)
            .unwrap()
            .unwrap();
        store.ingest(chain_event, 12345);

        let bucket = store.bucket(&ObjectId::new([0x99; 32])).unwrap();
        assert_eq!(bucket.strike, 50_000_000_000);
        assert_eq!(bucket.asset_type.as_str(), "BTC");
        assert_eq!(bucket.settlement_type.as_str(), "USDC");
    }
}
