//! Read model maintained from indexer events.

use std::collections::BTreeMap;

use parking_lot::RwLock;
use tracing::{debug, trace};

use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};

use serde::{Deserialize, Serialize};

use crate::bucket::Bucket;
use crate::catalog::TokenCatalog;

/// Indexer checkpoint-ingestion progress, proxied from the indexer's
/// `GET /progress` for the Debug page (SO-107). Field names mirror the
/// indexer's `ProgressSnapshot`.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct IndexerProgress {
    pub start_checkpoint: u64,
    pub current_checkpoint: u64,
    /// `null` until the indexer has polled the chain tip at least once.
    pub tip_checkpoint: Option<u64>,
    pub rate_checkpoints_per_sec: f64,
    pub caught_up: bool,
}

// ── indexer GraphQL response (SO-97) ──────────────────────────────────────

#[derive(Deserialize)]
struct GqlResponse {
    data: Option<GqlData>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct GqlData {
    positions: Vec<IndexerPosition>,
}

/// One enriched position from the indexer GraphQL API. All on-chain integers
/// arrive as decimal strings to preserve precision; the handler scales them.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IndexerPosition {
    pub object_id: String,
    pub bucket_id: String,
    pub recipient: String,
    pub range_start_raw: String,
    pub range_end_raw: String,
    pub asset_type: String,
    pub settlement_type: String,
    pub strike_raw: String,
    pub strike_scale: i32,
    pub expiry_ms: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    pub premium_received_raw: String,
    pub mm_account_id: String,
    pub tx_digest: String,
    pub minted_at_ms: String,
}

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
    /// SO-97: client for the indexer GraphQL query API, used to enrich the
    /// Dashboard's wallet-direct positions.
    http: reqwest::Client,
    indexer_graphql_url: String,
    /// SO-107: the indexer's `GET /progress` URL, derived from the GraphQL URL
    /// (same host:port, `/graphql` → `/progress`).
    indexer_progress_url: String,
}

impl AppState {
    pub fn new(catalog: TokenCatalog, indexer_graphql_url: String) -> Self {
        let base = indexer_graphql_url
            .strip_suffix("/graphql")
            .unwrap_or(&indexer_graphql_url)
            .trim_end_matches('/');
        let indexer_progress_url = format!("{base}/progress");
        Self {
            buckets: RwLock::new(BTreeMap::new()),
            positions: RwLock::new(BTreeMap::new()),
            lots_by_recipient: RwLock::new(BTreeMap::new()),
            catalog,
            http: reqwest::Client::new(),
            indexer_graphql_url,
            indexer_progress_url,
        }
    }

    /// Proxy the indexer's checkpoint-ingestion progress for the Debug page.
    pub async fn query_indexer_progress(&self) -> anyhow::Result<IndexerProgress> {
        let resp = self
            .http
            .get(&self.indexer_progress_url)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Query the indexer's GraphQL `positions(objectIds)` for enrichment.
    /// Returns the rows the indexer knows about; unknown ids are simply
    /// absent (the caller renders those degraded).
    pub async fn query_indexer_positions(
        &self,
        object_ids: &[String],
    ) -> anyhow::Result<Vec<IndexerPosition>> {
        if object_ids.is_empty() {
            return Ok(vec![]);
        }
        const QUERY: &str = r#"query($ids:[String!]!){positions(objectIds:$ids){objectId bucketId recipient rangeStartRaw rangeEndRaw assetType settlementType strikeRaw strikeScale expiryMs totalWrittenRaw exerciseCursorRaw premiumReceivedRaw mmAccountId txDigest mintedAtMs}}"#;
        let body = serde_json::json!({ "query": QUERY, "variables": { "ids": object_ids } });
        let resp = self
            .http
            .post(&self.indexer_graphql_url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: GqlResponse = resp.json().await?;
        if let Some(errors) = parsed.errors {
            if !errors.is_empty() {
                anyhow::bail!("indexer graphql errors: {:?}", errors);
            }
        }
        Ok(parsed.data.map(|d| d.positions).unwrap_or_default())
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
                        invalidated: false,
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
            ChainEvent::BucketInvalidated(i) => {
                if let Some(v) = self.buckets.write().get_mut(&i.bucket_id) {
                    v.invalidated = true;
                }
            }
            ChainEvent::BucketRevalidated(r) => {
                if let Some(v) = self.buckets.write().get_mut(&r.bucket_id) {
                    v.invalidated = false;
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
    use protocol_types::events::{
        BucketCleaned, BucketCreated, BucketInvalidated, BucketRevalidated,
    };
    use protocol_types::ids::SuiAddress;

    fn evt(seq: u64, ev: ChainEvent) -> IndexedEvent {
        IndexedEvent {
            sequence: seq,
            timestamp_ms: 0,
            event: ev,
        }
    }

    #[test]
    fn bucket_lifecycle() {
        let s = AppState::new(TokenCatalog::default(), "http://127.0.0.1:9002/graphql".to_string());
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

    #[test]
    fn invalidation_toggle_surfaces_on_active_bucket() {
        let s = AppState::new(TokenCatalog::default(), "http://127.0.0.1:9002/graphql".to_string());
        let id = ObjectId::new([0xab; 32]);
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
        assert!(!s.active_buckets()[0].1.invalidated);

        s.ingest_event(&evt(
            2,
            ChainEvent::BucketInvalidated(BucketInvalidated {
                bucket_id: id,
                at_ms: 10,
                admin: SuiAddress::new([0xaa; 32]),
                reason: b"oops".to_vec(),
            }),
        ));
        assert!(s.active_buckets()[0].1.invalidated);

        s.ingest_event(&evt(
            3,
            ChainEvent::BucketRevalidated(BucketRevalidated {
                bucket_id: id,
                at_ms: 20,
                admin: SuiAddress::new([0xaa; 32]),
                reason: b"undo".to_vec(),
            }),
        ));
        assert!(!s.active_buckets()[0].1.invalidated);
    }
}
