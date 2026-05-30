//! Read model maintained from indexer events.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tracing::{debug, trace};

use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::bucket::Bucket;
use crate::catalog::TokenCatalog;

/// One live `Position` object held by some wallet. Tracked by `object_id`
/// because that's the stable identity across transfers. `recipient` is the
/// owner-of-record at mint time and may go stale if the writer transfers
/// the NFT P2P (transfer-walking is a follow-up).
///
/// `premium_received` and `mm_account_id` are captured at mint so the
/// writer-side dashboard can show "earned X USDC, sold to MM Y" without
/// an extra lookup. `mm_account_id` is the `signer_account_id` from the
/// `WriteExecuted` — the trader MM in writer flow, the writer MM itself
/// in trader flow (where the MM is the one who holds the Position).
#[derive(Clone, Debug)]
pub struct Position {
    pub bucket_id: ObjectId,
    pub recipient: SuiAddress,
    pub range_start: u128,
    pub range_end: u128,
    pub premium_received: u64,
    pub mm_account_id: ObjectId,
    pub timestamp_ms: u64,
}

/// One `WriteExecuted` event, persisted as a per-buyer purchase lot so the
/// dashboard can show provenance (`boughtFrom`, `premiumPaid`, `boughtAt`)
/// for owned `CallOption` objects. Append-only — no removal on exercise.
#[derive(Clone, Debug)]
pub struct CallTokenLot {
    pub bucket_id: ObjectId,
    pub call_option_id: ObjectId,
    pub recipient: SuiAddress,
    pub seller_account_id: ObjectId,
    pub amount: u64,
    pub premium_paid: u64,
    pub timestamp_ms: u64,
    pub sequence: u64,
}

pub struct AppState {
    buckets: RwLock<BTreeMap<ObjectId, Bucket>>,
    /// Keyed by Position object id.
    positions: RwLock<BTreeMap<ObjectId, Position>>,
    /// Keyed by recipient address. Sorted by sequence (newest last).
    lots_by_recipient: RwLock<BTreeMap<SuiAddress, Vec<CallTokenLot>>>,
    pub catalog: TokenCatalog,
}

impl AppState {
    pub fn new(catalog: TokenCatalog) -> Self {
        Self {
            buckets: RwLock::new(BTreeMap::new()),
            positions: RwLock::new(BTreeMap::new()),
            lots_by_recipient: RwLock::new(BTreeMap::new()),
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

    /// Snapshot of buckets indexed by id — used by handlers that need to
    /// join positions/lots to their bucket. Returns even cleaned buckets;
    /// callers can filter as they see fit.
    pub fn buckets_by_id(&self) -> BTreeMap<ObjectId, Bucket> {
        self.buckets.read().clone()
    }

    /// All `Position` objects currently held by `wallet`. Filters by the
    /// mint-time recipient. Note: may include stale entries if the user
    /// has transferred the NFT to another wallet (transfer-walking is a
    /// follow-up — see `crate::handlers::positions` doc comment).
    pub fn positions_for_recipient(&self, wallet: &SuiAddress) -> Vec<(ObjectId, Position)> {
        self.positions
            .read()
            .iter()
            .filter(|(_, p)| p.recipient == *wallet)
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn lots_for_recipient(&self, wallet: &SuiAddress) -> Vec<CallTokenLot> {
        self.lots_by_recipient
            .read()
            .get(wallet)
            .cloned()
            .unwrap_or_default()
    }
}

impl indexer_client::EventSink for AppState {
    fn ingest_event(&self, indexed: &IndexedEvent) {
        trace!(sequence = indexed.sequence, "ingesting indexer event");
        match &indexed.event {
            ChainEvent::BucketCreated(b) => {
                debug!(
                    bucket = %b.bucket_id,
                    asset_type = %b.asset_type,
                    settlement_type = %b.settlement_type,
                    strike = %b.strike,
                    strike_scale = b.strike_scale,
                    expiry_ms = b.expiry_ms,
                    "BucketCreated"
                );
                self.buckets.write().insert(
                    b.bucket_id,
                    Bucket {
                        asset_type: b.asset_type.clone(),
                        settlement_type: b.settlement_type.clone(),
                        strike: b.strike,
                        strike_scale: b.strike_scale,
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
                self.positions.write().insert(
                    w.position_id,
                    Position {
                        bucket_id: w.bucket_id,
                        recipient: w.position_recipient,
                        range_start: w.range_start,
                        range_end: w.range_end,
                        premium_received: w.gross_premium,
                        mm_account_id: w.signer_account_id,
                        timestamp_ms: indexed.timestamp_ms,
                    },
                );
                self.lots_by_recipient
                    .write()
                    .entry(w.call_token_recipient)
                    .or_default()
                    .push(CallTokenLot {
                        bucket_id: w.bucket_id,
                        call_option_id: w.call_option_id,
                        recipient: w.call_token_recipient,
                        seller_account_id: w.signer_account_id,
                        amount: w.write_amount,
                        premium_paid: w.gross_premium,
                        timestamp_ms: indexed.timestamp_ms,
                        sequence: indexed.sequence,
                    });
            }
            ChainEvent::Exercised(e) => {
                if let Some(v) = self.buckets.write().get_mut(&e.bucket_id) {
                    v.exercise_cursor = e.cursor_after;
                }
            }
            ChainEvent::Redeemed(r) => {
                self.positions.write().remove(&r.position_id);
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
    use indexer_client::EventSink;
    use protocol_types::asset::AssetType;
    use protocol_types::events::{BucketCleaned, BucketCreated};

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
                strike_scale: 0,
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
