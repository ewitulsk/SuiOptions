//! All mutable shared state lives here. The WS handlers and RFQ orchestrator
//! both work against [`AppState`] — never against raw fields — so any future
//! refactor of how data is stored doesn't ripple out.
//!
//! Signing keys and bucket state are NOT mirrored here: they're read
//! just-in-time from the indexer's GraphQL API
//! ([`indexer_graphql::IndexerClient`]). Balance tracking and reservations
//! are gone entirely (collateral abstraction, plan §7): a collateral
//! implementation need not have a readable balance at all, so enforcement
//! lives where it always really was — the on-chain revert — and the
//! reputation system is the quality filter. The only state this service owns
//! is the seen-nonce table and the per-MM reputation score.

pub mod mm_registry;
pub mod nonces;
pub mod reputation;

use std::sync::Arc;

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, Semaphore};

use protocol_types::bucket_spec::BucketSpec;
use protocol_types::ids::ObjectId;
use protocol_types::sides::Side;

pub use indexer_graphql::{Account, Bucket, IndexerClient};
pub use mm_registry::{MmConnection, MmRegistry};
pub use nonces::{InsertOutcome, NonceTable};
pub use reputation::{ReputationStats, ReputationStore};

/// Routed MM response — populated by the MM read task, drained by the RFQ
/// orchestrator's matcher (or the bulk-view collector).
#[derive(Debug)]
pub enum MmResponse {
    Quote(ObjectId, protocol_types::messages::MmQuotePayload),
    Decline(ObjectId),
    /// Unsigned indicative premiums for a bulk-view RFQ. Routed through the
    /// same `pending_rfqs` channel keyed by request_id.
    BulkView(ObjectId, protocol_types::messages::BulkViewQuotePayload),
}

/// One cached bulk-view premium for a `(bucket_id, write_amount)` pair.
#[derive(Clone, Copy, Debug)]
pub struct BulkViewCacheEntry {
    /// Mean of responding MMs' premiums, settlement smallest-units.
    pub premium: u64,
    pub mm_count: u32,
    /// When this value was fetched (unix ms). Drives TTL/staleness.
    pub cached_at_ms: u64,
}

/// One-line observation of a retail `RFQRequest` arriving at the service.
/// Pushed onto [`AppState::rfq_observers`] for operator-facing tooling
/// (e.g. the `rfq-monitor` binary).
#[derive(Clone, Debug, Serialize)]
pub struct RfqObservation {
    pub timestamp_ms: u64,
    pub request_id: String,
    /// What was asked about. An RFQ names economics, not an object, so the
    /// observer feed reports the spec — including for strikes whose bucket
    /// does not exist yet, which are exactly the ones worth watching.
    pub spec: BucketSpec,
    pub write_amount: u64,
    pub side: Side,
}

pub struct AppState {
    /// Seen `(signer, nonce)` pairs — the nonce-unseen validation check.
    pub nonces: NonceTable,
    pub reputation: ReputationStore,
    pub mms: MmRegistry,
    /// JIT client for indexer signer/bucket/event reads.
    pub indexer: IndexerClient,
    /// Per-signer high-water sequence for `WriteExecuted` reconciliation —
    /// the cursor [`record_fills`](Self::record_fills) advances so each pass
    /// only scans new executions. Feeds the reputation fill-rate.
    reconcile_cursors: DashMap<ObjectId, u64>,
    /// `request_id → matcher_tx`. Populated when an RFQ is broadcast,
    /// removed when the RFQ completes. The MM read task looks up its
    /// `Quote`/`Decline` response's `request_id` here to route it.
    pub pending_rfqs: DashMap<String, mpsc::Sender<MmResponse>>,
    /// Cached bulk-view premiums keyed by `(bucket_id, write_amount)`. Served
    /// stale-while-revalidate: a hit older than the TTL is returned
    /// immediately while a background refresh re-broadcasts to MMs.
    pub bulk_view_cache: DashMap<(BucketSpec, u64), BulkViewCacheEntry>,
    /// Spec → bucket resolution, cached. Buckets are created just-in-time, so
    /// a spec with no bucket is a normal, quotable state rather than an error.
    pub specs: crate::rfq::resolve::SpecResolver,
    /// In-flight bulk-view refreshes, keyed the same as the cache. Presence
    /// means some task is already re-broadcasting that key, so others skip it
    /// (single-flight — "only send to MMs if a new request came in AND no
    /// refresh is already running").
    bulk_view_refreshing: DashMap<(BucketSpec, u64), ()>,
    /// Global cap on concurrent RFQ orchestrations across all retail
    /// connections. Acquired by `retail.rs` with `try_acquire_owned`;
    /// a saturated permit count means we reject the RFQ with
    /// `Error{code:"rate_limited"}` instead of spawning another task.
    pub rfq_global_inflight: Arc<Semaphore>,
    /// Fan-out for observer tooling. The retail handler publishes one
    /// [`RfqObservation`] per incoming RFQ; subscribers are best-effort and
    /// may miss messages under backlog (the broadcast channel drops oldest).
    pub rfq_observers: broadcast::Sender<RfqObservation>,
}

impl AppState {
    pub fn with_global_rfq_cap(cap: usize, indexer_graphql_url: String) -> Self {
        let (rfq_observers, _) = broadcast::channel(256);
        Self {
            nonces: NonceTable::default(),
            reputation: ReputationStore::default(),
            mms: MmRegistry::default(),
            indexer: IndexerClient::new(indexer_graphql_url),
            reconcile_cursors: DashMap::new(),
            pending_rfqs: DashMap::new(),
            bulk_view_cache: DashMap::new(),
            specs: crate::rfq::resolve::SpecResolver::new(),
            bulk_view_refreshing: DashMap::new(),
            rfq_global_inflight: Arc::new(Semaphore::new(cap)),
            rfq_observers,
        }
    }

    /// Try to claim the single-flight refresh slot for a `(bucket, amount)`
    /// key. Returns true if this caller now owns the refresh (must call
    /// [`release_bulk_view_refresh`](Self::release_bulk_view_refresh) when
    /// done); false if another task already holds it.
    pub fn try_claim_bulk_view_refresh(&self, key: (BucketSpec, u64)) -> bool {
        self.bulk_view_refreshing.insert(key, ()).is_none()
    }

    pub fn release_bulk_view_refresh(&self, key: &(BucketSpec, u64)) {
        self.bulk_view_refreshing.remove(key);
    }

    /// Record each of this signer's `WriteExecuted` fills the indexer now
    /// reports as executed for reputation (the fill-rate half of the
    /// composite score — the revert half is the on-chain revert the plan
    /// leans on). We scan the indexer's event log (via GraphQL) for this
    /// signer since our last cursor. Reservations are gone; this exists
    /// purely so `quotes_executed` keeps counting.
    pub async fn record_fills(&self, signer: ObjectId) -> anyhow::Result<()> {
        let after = self
            .reconcile_cursors
            .get(&signer)
            .map(|c| *c)
            .unwrap_or(0);
        let executed = self
            .indexer
            .write_executed_for_account_since(signer, after)
            .await?;
        let mut max_seq = after;
        for (seq, _nonce) in executed {
            self.reputation.record_executed(signer);
            max_seq = max_seq.max(seq);
        }
        if max_seq > after {
            self.reconcile_cursors.insert(signer, max_seq);
        }
        Ok(())
    }
}
