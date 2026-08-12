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

use crate::db::models::event_type_tag;
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
        packages: event_types::PackageIds<'_>,
        deepbook_original_package_id: Option<&str>,
        progress: Arc<ProgressState>,
    ) -> Self {
        let types = EventTypes::for_packages(packages, deepbook_original_package_id);
        info!(
            core = packages.core,
            auction = packages.auction.unwrap_or("<retired>"),
            rfq = packages.rfq.unwrap_or("<retired>"),
            vault = packages.vault.unwrap_or("<deprecated, SO-332>"),
            "indexer worker listening for events"
        );
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
/// (canonical call_type → (bucket_id, settlement_type)) since the store only
/// applies events at stage time. The map is keyed by the *canonical*
/// (`0x`-prefixed, padded) call_type so it matches the pool's type-string form.
fn resolve_deepbook_pool(
    store: &Store,
    local_buckets: &std::collections::HashMap<
        String,
        (protocol_types::ids::ObjectId, protocol_types::asset::AssetType),
    >,
    partial: event_types::DeepBookPoolCreatedPartial,
) -> Option<protocol_types::events::ChainEvent> {
    // A pool's base/quote come from the event type string (`0x`-prefixed),
    // while bucket call/settlement types are chain `TypeName`s (no `0x`).
    // Canonicalize before every comparison so the two forms match (SO-163).
    let base_canonical = partial.base_asset_type.to_canonical();
    let (bucket_id, settlement_type) = local_buckets
        .get(&base_canonical)
        .cloned()
        .or_else(|| {
            store
                .bucket_by_call_type(&partial.base_asset_type)
                .map(|(id, b)| (id, b.settlement_type))
        })?;
    if partial.quote_asset_type.to_canonical() != settlement_type.to_canonical() {
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

/// Promote a decoded DeepBook `OrderFilled` into a `ChainEvent` if its pool is
/// one of OUR bucket venues; `None` for fills on foreign pools (SO-209). A
/// same-checkpoint `PoolCreated` is visible via `local_pools`.
fn resolve_deepbook_fill(
    store: &Store,
    local_pools: &std::collections::HashMap<
        protocol_types::ids::ObjectId,
        protocol_types::ids::ObjectId,
    >,
    partial: event_types::DeepBookOrderFilledPartial,
) -> Option<protocol_types::events::ChainEvent> {
    let bucket_id = local_pools
        .get(&partial.pool_id)
        .copied()
        .or_else(|| store.bucket_by_pool_id(&partial.pool_id))?;
    Some(protocol_types::events::ChainEvent::DeepBookOrderFilled(
        protocol_types::events::DeepBookOrderFilled {
            pool_id: partial.pool_id,
            bucket_id,
            taker_balance_manager_id: partial.taker_balance_manager_id,
            maker_balance_manager_id: partial.maker_balance_manager_id,
            taker_is_bid: partial.taker_is_bid,
            base_quantity: partial.base_quantity,
            quote_quantity: partial.quote_quantity,
            price: partial.price,
            taker_fee: partial.taker_fee,
            taker_fee_is_deep: partial.taker_fee_is_deep,
            maker_fee: partial.maker_fee,
            maker_fee_is_deep: partial.maker_fee_is_deep,
            timestamp_ms: partial.timestamp_ms,
        },
    ))
}

#[async_trait]
impl Worker for ProtocolEventWorker {
    type Result = ();

    async fn process_checkpoint(&self, checkpoint: &CheckpointData) -> Result<()> {
        let seq = checkpoint.checkpoint_summary.sequence_number;
        let ts_ms = checkpoint.checkpoint_summary.timestamp_ms;

        // One trace per checkpoint (SO-180). The body below is fully
        // synchronous (no awaits), so holding the entered guard is safe.
        let _cp = tracing::info_span!("checkpoint", seq).entered();

        // Decode pass — collect everything we recognise in checkpoint order
        // before touching the store. Keeps the lock window in step 2 small.
        let mut decoded: Vec<(protocol_types::events::ChainEvent, String, i32)> =
            Vec::new();
        // Buckets created earlier in THIS checkpoint, so a PoolCreated in the
        // same checkpoint can still resolve (the store only applies events at
        // stage time). call_type → (bucket_id, settlement_type).
        let mut local_buckets: std::collections::HashMap<_, _> = Default::default();
        // Pools created earlier in THIS checkpoint, so a same-checkpoint
        // OrderFilled can resolve its bucket (SO-209). pool_id → bucket_id.
        let mut local_pools: std::collections::HashMap<
            protocol_types::ids::ObjectId,
            protocol_types::ids::ObjectId,
        > = Default::default();
        for tx in &checkpoint.transactions {
            let Some(events) = &tx.events else { continue };
            let tx_digest = tx.transaction.digest().base58_encode();
            for (idx, event) in events.data.iter().enumerate() {
                // Canonical form (padded addresses): `Display` strips
                // leading zeros, so a package id like 0x0909… never
                // byte-matches the padded ids token-info serves (the
                // move-type-normalization rule; bit us on the SO-299
                // trading_vault publish).
                let type_str = event.type_.to_canonical_string(true);
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
                            // Key by the canonical call_type so a same-checkpoint
                            // pool (type-string form, `0x`-prefixed) still hits.
                            local_buckets.insert(
                                b.call_type.to_canonical(),
                                (b.bucket_id, b.settlement_type.clone()),
                            );
                        }
                        metrics::counter!(
                            "indexer_events_decoded_total",
                            "event_type" => event_type_tag(&parsed),
                        )
                        .increment(1);
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
                            if let protocol_types::events::ChainEvent::DeepBookPoolCreated(p) = &ev {
                                local_pools.insert(p.pool_id, p.bucket_id);
                            }
                            debug!(
                                checkpoint = seq,
                                tx = %tx_digest,
                                event_idx = idx,
                                event = ?ev,
                                "picked up DeepBook pool for a bucket call coin"
                            );
                            metrics::counter!(
                                "indexer_events_decoded_total",
                                "event_type" => event_type_tag(&ev),
                            )
                            .increment(1);
                            decoded.push((ev, tx_digest.clone(), idx as i32));
                        }
                        continue;
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
                        continue;
                    }
                }
                // …or a DeepBook OrderFilled on one of our bucket pools (SO-209).
                // Fills on foreign pools resolve to None and are dropped.
                match event_types::parse_deepbook_order_filled(
                    &self.types,
                    &type_str,
                    &event.contents,
                ) {
                    Ok(Some(partial)) => {
                        if let Some(ev) =
                            resolve_deepbook_fill(&self.store, &local_pools, partial)
                        {
                            debug!(
                                checkpoint = seq,
                                tx = %tx_digest,
                                event_idx = idx,
                                event = ?ev,
                                "picked up DeepBook fill on a bucket pool"
                            );
                            metrics::counter!(
                                "indexer_events_decoded_total",
                                "event_type" => event_type_tag(&ev),
                            )
                            .increment(1);
                            decoded.push((ev, tx_digest.clone(), idx as i32));
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        error!(
                            error = %e,
                            tx = %tx_digest,
                            event_type = %type_str,
                            "failed to decode DeepBook OrderFilled; skipping"
                        );
                    }
                }
            }
        }

        // Stage → persist. If the DB write fails, return Err so the framework
        // retries; on a hard crash, boot-time hydration from `indexer_progress`
        // corrects any in-memory drift.
        let staged = self.store.stage_batch(seq, ts_ms, decoded)?;
        let apply_start = std::time::Instant::now();
        self.repo
            .apply_checkpoint(&staged.db_batch)
            .with_context(|| format!("persisting checkpoint {seq}"))?;
        metrics::histogram!("indexer_checkpoint_apply_duration_seconds")
            .record(apply_start.elapsed().as_secs_f64());
        metrics::gauge!("indexer_checkpoint_height").set(seq as f64);
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
        let types = EventTypes::for_packages(
            event_types::PackageIds { core: "0xabc", auction: Some("0xa1"), rfq: Some("0xf1"), vault: Some("0xe1"), trading_vault: None, deepbook_adapter: None, options_adapter: None, exchange_adapter: None, equity_oracle: None },
            None,
        );
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
            AssetType::new("0x9::call_1::CALL_1").to_canonical(),
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

    /// Real chain forms disagree on the `0x` prefix: `BucketCreated`'s
    /// `TypeName` fields arrive WITHOUT `0x` (`93c0…::call_1::CALL_1`), while a
    /// DeepBook `PoolCreated`'s base/quote are parsed from the event type
    /// string WITH `0x` (`0x93c0…::call_1::CALL_1`). Resolution must
    /// canonicalize both sides — before this fix the byte-exact compare never
    /// matched and every pool was silently dropped as "foreign".
    #[test]
    fn deepbook_pool_resolves_across_0x_prefix_mismatch() {
        let call_chain =
            "93c0cc25b8a167a537e3f116cdc339a61e7dd25355cc3c6f640362f41d0f6d78::call_1::CALL_1";
        let usdc_chain =
            "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tusdc::TUSDC";
        let call_0x = format!("0x{call_chain}");
        let usdc_0x = format!("0x{usdc_chain}");

        // Store path: bucket persisted with chain-form (no `0x`) types; pool
        // partial carries the `0x`-prefixed forms from the event type string.
        let store = Store::new();
        store.ingest(
            protocol_types::events::ChainEvent::BucketCreated(BucketCreated {
                bucket_id: ObjectId::new([0x33; 32]),
                asset_type: AssetType::new("tbtc"),
                settlement_type: AssetType::new(usdc_chain),
                call_type: AssetType::new(call_chain),
                expiry_ms: 1,
                strike: 1,
                strike_scale: 0,
            }),
            1,
        );
        let local = Default::default();
        let ev = resolve_deepbook_pool(&store, &local, partial(&call_0x, &usdc_0x))
            .expect("store-resolved pool matches across 0x-prefix mismatch");
        match ev {
            protocol_types::events::ChainEvent::DeepBookPoolCreated(p) => {
                assert_eq!(p.bucket_id, ObjectId::new([0x33; 32]));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Same-checkpoint path: the worker keys the local map by the
        // canonical call_type, so the lookup still matches the 0x-form pool.
        let fresh = Store::new();
        let mut local2 = std::collections::HashMap::new();
        local2.insert(
            call_0x.clone(),
            (ObjectId::new([0x44; 32]), AssetType::new(&usdc_0x)),
        );
        let ev = resolve_deepbook_pool(&fresh, &local2, partial(&call_0x, &usdc_0x))
            .expect("local-resolved pool matches across 0x-prefix mismatch");
        match ev {
            protocol_types::events::ChainEvent::DeepBookPoolCreated(p) => {
                assert_eq!(p.bucket_id, ObjectId::new([0x44; 32]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
