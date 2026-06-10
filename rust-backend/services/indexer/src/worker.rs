//! Sui checkpoint Worker.
//!
//! Implements [`sui_data_ingestion_core::Worker`]. The framework hands us
//! `CheckpointData` instances in order; we walk every transaction's emitted
//! events, filter to the type strings we recognize (see
//! [`crate::event_types`]), BCS-deserialize into the `protocol_types::events`
//! mirror structs, and persist them.
//!
//! Per checkpoint:
//!   1. Decode + collect every recognised event in checkpoint order.
//!   2. [`Store::stage_batch`] under a single lock: applies materialised-view
//!      mutations, assigns monotonic sequences, builds a [`CheckpointBatch`].
//!   3. [`Repo::apply_checkpoint`] writes the batch in one transaction, after
//!      which the materialized views and Postgres are consistent.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::{debug, error, info};

use sui_data_ingestion_core::Worker;
use sui_types::full_checkpoint_content::CheckpointData;

use crate::db::Repo;
use crate::event_types::{self, EventTypes};
use crate::progress::ProgressState;
use crate::store::Store;

pub struct ProtocolEventWorker {
    store: Arc<Store>,
    repo: Repo,
    types: EventTypes,
    progress: Arc<ProgressState>,
}

impl ProtocolEventWorker {
    pub fn new(
        store: Arc<Store>,
        repo: Repo,
        package_id: &str,
        deepbook_original_package_id: Option<&str>,
        progress: Arc<ProgressState>,
    ) -> Self {
        let types = EventTypes::for_package(package_id, deepbook_original_package_id);
        info!(package_id, "indexer worker listening for events");
        for t in types.all_strings() {
            debug!(event_type = t, "subscribed");
        }
        if let Some(prefix) = &types.deepbook_pool_created_prefix {
            debug!(event_type = %format!("{prefix}…>"), "subscribed (deepbook, prefix match)");
        }
        Self { store, repo, types, progress }
    }
}

/// Promote a decoded DeepBook `PoolCreated` into a `ChainEvent` if its base
/// asset is one of OUR bucket call coins; `None` for foreign pools. Buckets
/// created earlier in the same checkpoint are visible via `local_buckets`
/// (call_type → (bucket_id, settlement_type)) since the store only applies
/// events at stage time.
fn resolve_deepbook_pool(
    store: &Store,
    local_buckets: &std::collections::HashMap<
        protocol_types::asset::AssetType,
        (protocol_types::ids::ObjectId, protocol_types::asset::AssetType),
    >,
    partial: event_types::DeepBookPoolCreatedPartial,
) -> Option<protocol_types::events::ChainEvent> {
    let (bucket_id, settlement_type) = local_buckets
        .get(&partial.base_asset_type)
        .cloned()
        .or_else(|| {
            store
                .bucket_by_call_type(&partial.base_asset_type)
                .map(|(id, b)| (id, b.settlement_type))
        })?;
    if partial.quote_asset_type != settlement_type {
        tracing::warn!(
            pool = %partial.pool_id.to_hex(),
            bucket = %bucket_id.to_hex(),
            quote = %partial.quote_asset_type.as_str(),
            settlement = %settlement_type.as_str(),
            "DeepBook pool quotes a bucket call coin against the wrong asset; ignoring"
        );
        return None;
    }
    Some(protocol_types::events::ChainEvent::DeepBookPoolCreated(
        protocol_types::events::DeepBookPoolCreated {
            pool_id: partial.pool_id,
            bucket_id,
            base_asset_type: partial.base_asset_type,
            quote_asset_type: partial.quote_asset_type,
            tick_size: partial.tick_size,
            lot_size: partial.lot_size,
            min_size: partial.min_size,
            taker_fee: partial.taker_fee,
            maker_fee: partial.maker_fee,
        },
    ))
}

#[async_trait]
impl Worker for ProtocolEventWorker {
    type Result = ();

    async fn process_checkpoint(&self, checkpoint: &CheckpointData) -> Result<()> {
        let seq = checkpoint.checkpoint_summary.sequence_number;
        let ts_ms = checkpoint.checkpoint_summary.timestamp_ms;

        // Decode pass — collect everything we recognise in checkpoint order
        // before touching the store. Keeps the lock window in step 2 small.
        let mut decoded: Vec<(protocol_types::events::ChainEvent, String, i32)> =
            Vec::new();
        // Buckets created earlier in THIS checkpoint, so a PoolCreated in the
        // same checkpoint can still resolve (the store only applies events at
        // stage time). call_type → (bucket_id, settlement_type).
        let mut local_buckets: std::collections::HashMap<_, _> = Default::default();
        for tx in &checkpoint.transactions {
            let Some(events) = &tx.events else { continue };
            let tx_digest = tx.transaction.digest().base58_encode();
            for (idx, event) in events.data.iter().enumerate() {
                let type_str = event.type_.to_string();
                match event_types::dispatch(&self.types, &type_str, &event.contents) {
                    Ok(Some(parsed)) => {
                        debug!(
                            checkpoint = seq,
                            tx = %tx_digest,
                            event_idx = idx,
                            event_type = %type_str,
                            event = ?parsed,
                            "picked up event"
                        );
                        if let protocol_types::events::ChainEvent::BucketCreated(b) = &parsed {
                            local_buckets.insert(
                                b.call_type.clone(),
                                (b.bucket_id, b.settlement_type.clone()),
                            );
                        }
                        decoded.push((parsed, tx_digest.clone(), idx as i32));
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        error!(
                            error = %e,
                            tx = %tx_digest,
                            event_type = %type_str,
                            "BCS decode of known event type failed — schema drift?"
                        );
                        continue;
                    }
                }
                // Not a protocol event — maybe a DeepBook PoolCreated for one
                // of our call coins (SO-152). Foreign pools resolve to None.
                match event_types::parse_deepbook_pool_created(
                    &self.types,
                    &type_str,
                    &event.contents,
                ) {
                    Ok(Some(partial)) => {
                        if let Some(ev) =
                            resolve_deepbook_pool(&self.store, &local_buckets, partial)
                        {
                            debug!(
                                checkpoint = seq,
                                tx = %tx_digest,
                                event_idx = idx,
                                event = ?ev,
                                "picked up DeepBook pool for a bucket call coin"
                            );
                            decoded.push((ev, tx_digest.clone(), idx as i32));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Third-party event garbage must never stall the
                        // pipeline — log and move on.
                        error!(
                            error = %e,
                            tx = %tx_digest,
                            event_type = %type_str,
                            "failed to decode DeepBook PoolCreated; skipping"
                        );
                    }
                }
            }
        }

        // Stage → persist. If the DB write fails, return Err so the framework
        // retries; on a hard crash, boot-time hydration from `indexer_progress`
        // corrects any in-memory drift.
        let staged = self.store.stage_batch(seq, ts_ms, decoded)?;
        self.repo
            .apply_checkpoint(&staged.db_batch)
            .with_context(|| format!("persisting checkpoint {seq}"))?;
        self.progress.record_checkpoint(seq);

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
    use protocol_types::asset::AssetType;
    use protocol_types::events::BucketCreated;
    use protocol_types::ids::ObjectId;

    /// Sanity-check that an event the worker would receive via Sui's framework
    /// (BCS bytes + matching type string) round-trips through the dispatch
    /// fn into our in-memory store with all fields intact. The full Worker
    /// path additionally writes to Postgres — covered in integration tests
    /// that spin up a real DB; this one stays unit-level by exercising the
    /// dispatch + Store::ingest fast path directly.
    #[test]
    fn dispatch_round_trips_a_bucket_created_event_into_store() {
        let pkg = "0xabc";
        let types = EventTypes::for_package(pkg, None);
        let store = Store::new();

        let evt = BucketCreated {
            bucket_id: ObjectId::new([0x99; 32]),
            asset_type: AssetType::new("BTC"),
            settlement_type: AssetType::new("USDC"),
            call_type: AssetType::new("0x9::call_0::CALL_0"),
            expiry_ms: 1_700_000_000_000,
            strike: 50_000_000_000,
            strike_scale: 2,
        };
        let bytes = bcs::to_bytes(&evt).unwrap();
        let chain_event = event_types::dispatch(&types, &types.bucket_created, &bytes)
            .unwrap()
            .unwrap();
        store.ingest(chain_event, 12345);

        let bucket = store.bucket(&ObjectId::new([0x99; 32])).unwrap();
        assert_eq!(bucket.strike, 50_000_000_000);
        assert_eq!(bucket.strike_scale, 2);
        assert_eq!(bucket.asset_type.as_str(), "BTC");
        assert_eq!(bucket.settlement_type.as_str(), "USDC");
    }

    fn partial(base: &str, quote: &str) -> event_types::DeepBookPoolCreatedPartial {
        event_types::DeepBookPoolCreatedPartial {
            pool_id: ObjectId::new([0xaa; 32]),
            base_asset_type: AssetType::new(base),
            quote_asset_type: AssetType::new(quote),
            tick_size: 10_000,
            lot_size: 1_000,
            min_size: 10_000,
            taker_fee: 1_000_000,
            maker_fee: 500_000,
        }
    }

    #[test]
    fn deepbook_pool_resolution_filters_to_our_buckets() {
        let store = Store::new();
        store.ingest(
            protocol_types::events::ChainEvent::BucketCreated(BucketCreated {
                bucket_id: ObjectId::new([0x11; 32]),
                asset_type: AssetType::new("TBTC"),
                settlement_type: AssetType::new("0x9::tusdc::TUSDC"),
                call_type: AssetType::new("0x9::call_0::CALL_0"),
                expiry_ms: 1,
                strike: 1,
                strike_scale: 0,
            }),
            1,
        );
        let local = Default::default();

        // Base matches a known call coin + quote matches settlement → resolved.
        let ev = resolve_deepbook_pool(
            &store,
            &local,
            partial("0x9::call_0::CALL_0", "0x9::tusdc::TUSDC"),
        )
        .expect("our pool resolves");
        match ev {
            protocol_types::events::ChainEvent::DeepBookPoolCreated(p) => {
                assert_eq!(p.bucket_id, ObjectId::new([0x11; 32]));
                assert_eq!(p.pool_id, ObjectId::new([0xaa; 32]));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Foreign base coin → dropped.
        assert!(resolve_deepbook_pool(&store, &local, partial("0x2::sui::SUI", "0x9::tusdc::TUSDC")).is_none());
        // Our call coin quoted against the wrong asset → dropped (warned).
        assert!(resolve_deepbook_pool(&store, &local, partial("0x9::call_0::CALL_0", "0x2::sui::SUI")).is_none());

        // Bucket created in the same checkpoint (not yet in store) resolves
        // via the local map.
        let fresh = Store::new();
        let mut local2 = std::collections::HashMap::new();
        local2.insert(
            AssetType::new("0x9::call_1::CALL_1"),
            (ObjectId::new([0x22; 32]), AssetType::new("0x9::tusdc::TUSDC")),
        );
        let ev = resolve_deepbook_pool(
            &fresh,
            &local2,
            partial("0x9::call_1::CALL_1", "0x9::tusdc::TUSDC"),
        )
        .expect("same-checkpoint bucket resolves");
        match ev {
            protocol_types::events::ChainEvent::DeepBookPoolCreated(p) => {
                assert_eq!(p.bucket_id, ObjectId::new([0x22; 32]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
