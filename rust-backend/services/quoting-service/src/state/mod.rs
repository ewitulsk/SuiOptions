//! All mutable shared state lives here. The WS handlers and RFQ orchestrator
//! both work against [`AppState`] — never against raw fields — so any future
//! refactor of how data is stored doesn't ripple out.
//!
//! Account balances, signing keys, and bucket state are NOT mirrored here:
//! they're read just-in-time from the indexer's GraphQL API
//! ([`indexer_graphql::IndexerClient`]). The only state this service owns is
//! what the indexer can't tell it: in-flight quote reservations and the
//! per-MM reputation score.

pub mod mm_registry;
pub mod reputation;
pub mod reservations;

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio::time::interval;
use tracing::warn;

use protocol_types::asset::AssetType;
use protocol_types::ids::ObjectId;
use protocol_types::sides::Side;

pub use indexer_graphql::{Account, Bucket, IndexerClient};
pub use token_info_client::VerifiedOrgsWatcher;
pub use mm_registry::{MmConnection, MmRegistry};
pub use reputation::{ReputationStats, ReputationStore};
pub use reservations::{InsertOutcome, Reservation, ReservationTable};

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
    pub bucket_id: ObjectId,
    pub write_amount: u64,
    pub side: Side,
}

pub struct AppState {
    pub reservations: ReservationTable,
    pub reputation: ReputationStore,
    pub mms: MmRegistry,
    /// JIT client for indexer account/bucket/event reads.
    pub indexer: IndexerClient,
    /// Per-account high-water sequence for `WriteExecuted` reconciliation —
    /// the cursor [`reconcile_executed`](Self::reconcile_executed) advances so
    /// each pass only scans new executions.
    reconcile_cursors: DashMap<ObjectId, u64>,
    /// `request_id → matcher_tx`. Populated when an RFQ is broadcast,
    /// removed when the RFQ completes. The MM read task looks up its
    /// `Quote`/`Decline` response's `request_id` here to route it.
    pub pending_rfqs: DashMap<String, mpsc::Sender<MmResponse>>,
    /// Cached bulk-view premiums keyed by `(bucket_id, write_amount)`. Served
    /// stale-while-revalidate: a hit older than the TTL is returned
    /// immediately while a background refresh re-broadcasts to MMs.
    pub bulk_view_cache: DashMap<(ObjectId, u64), BulkViewCacheEntry>,
    /// In-flight bulk-view refreshes, keyed the same as the cache. Presence
    /// means some task is already re-broadcasting that key, so others skip it
    /// (single-flight — "only send to MMs if a new request came in AND no
    /// refresh is already running").
    bulk_view_refreshing: DashMap<(ObjectId, u64), ()>,
    /// Global cap on concurrent RFQ orchestrations across all retail
    /// connections. Acquired by `retail.rs` with `try_acquire_owned`;
    /// a saturated permit count means we reject the RFQ with
    /// `Error{code:"rate_limited"}` instead of spawning another task.
    pub rfq_global_inflight: Arc<Semaphore>,
    /// Fan-out for observer tooling. The retail handler publishes one
    /// [`RfqObservation`] per incoming RFQ; subscribers are best-effort and
    /// may miss messages under backlog (the broadcast channel drops oldest).
    pub rfq_observers: broadcast::Sender<RfqObservation>,
    /// Verified-orgs allowlist (polled from token-info). RFQs against
    /// buckets of unverified orgs are refused — the verified-only surface.
    pub verified_orgs: VerifiedOrgsWatcher,
}

impl AppState {
    pub fn with_global_rfq_cap(
        cap: usize,
        indexer_graphql_url: String,
        verified_orgs: VerifiedOrgsWatcher,
    ) -> Self {
        let (rfq_observers, _) = broadcast::channel(256);
        Self {
            reservations: ReservationTable::default(),
            reputation: ReputationStore::default(),
            mms: MmRegistry::default(),
            indexer: IndexerClient::new(indexer_graphql_url),
            reconcile_cursors: DashMap::new(),
            pending_rfqs: DashMap::new(),
            bulk_view_cache: DashMap::new(),
            bulk_view_refreshing: DashMap::new(),
            rfq_global_inflight: Arc::new(Semaphore::new(cap)),
            rfq_observers,
            verified_orgs,
        }
    }

    /// Try to claim the single-flight refresh slot for a `(bucket, amount)`
    /// key. Returns true if this caller now owns the refresh (must call
    /// [`release_bulk_view_refresh`](Self::release_bulk_view_refresh) when
    /// done); false if another task already holds it.
    pub fn try_claim_bulk_view_refresh(&self, key: (ObjectId, u64)) -> bool {
        self.bulk_view_refreshing.insert(key, ()).is_none()
    }

    pub fn release_bulk_view_refresh(&self, key: (ObjectId, u64)) {
        self.bulk_view_refreshing.remove(&key);
    }

    /// Spendable balance for `(account, asset)`: the JIT-fetched on-chain
    /// balance minus our own active reservations in that asset. Callers must
    /// [`reconcile_executed`](Self::reconcile_executed) first so reservations
    /// whose write already landed (and is thus already reflected in the
    /// on-chain balance) don't get double-counted.
    pub fn available(&self, account: &Account, asset: &AssetType) -> u64 {
        account
            .balance(asset)
            .saturating_sub(self.reservations.active_amount(account.account_id, asset))
    }

    /// Release reservations whose `WriteExecuted` the indexer now reports, and
    /// record each as executed for reputation. We scan the indexer's event log
    /// (via GraphQL) for this account since our last cursor.
    ///
    /// NOTE (speed-up): this runs on demand during quote validation, so an
    /// idle MM's executed reservations aren't released until either it's quoted
    /// again or its TTL evicts it. A short-interval background reconcile would
    /// tighten that and is the obvious optimization if reservation-release
    /// latency ever matters.
    pub async fn reconcile_executed(&self, account: ObjectId) -> anyhow::Result<()> {
        let after = self
            .reconcile_cursors
            .get(&account)
            .map(|c| *c)
            .unwrap_or(0);
        let executed = self
            .indexer
            .write_executed_for_account_since(account, after)
            .await?;
        let mut max_seq = after;
        for (seq, nonce) in executed {
            if self.reservations.release(account, nonce).is_some() {
                self.reputation.record_executed(account);
            }
            max_seq = max_seq.max(seq);
        }
        if max_seq > after {
            self.reconcile_cursors.insert(account, max_seq);
        }
        Ok(())
    }
}

/// Background task: evict expired reservations every `tick`.
pub fn spawn_reservation_evictor(state: Arc<AppState>, tick: Duration) {
    tokio::spawn(async move {
        let mut iv = interval(tick);
        loop {
            iv.tick().await;
            let now = reservations::now_unix_ms();
            let evicted = state.reservations.evict_expired(now);
            for r in evicted {
                warn!(account = %r.account_id, nonce = r.nonce, "evicting expired reservation");
                state.reputation.record_expired_unused(r.account_id);
                // No need to mutate balance: the on-chain balance is
                // whatever the indexer reports; available re-derives on demand.
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn account_with(account_id: ObjectId, usdc: u64) -> Account {
        Account {
            account_id,
            owner: None,
            signing_scheme: Some(protocol_types::SigningScheme::Ed25519),
            signing_pubkey: vec![0xab; 32],
            balances: BTreeMap::from([(AssetType::new("USDC"), usdc)]),
        }
    }

    #[test]
    fn available_subtracts_active_reservations() {
        let s = AppState::with_global_rfq_cap(
            8,
            "http://127.0.0.1:1/graphql".into(),
            VerifiedOrgsWatcher::fixed(vec![]),
        );
        let account = ObjectId::new([0x01; 32]);
        let acct = account_with(account, 1_000);
        assert_eq!(s.available(&acct, &AssetType::new("USDC")), 1_000);

        s.reservations.insert(Reservation {
            account_id: account,
            nonce: 1,
            asset_type: AssetType::new("USDC"),
            amount: 300,
            valid_until_ms: 9_999,
            created_at_ms: 0,
        });
        assert_eq!(s.available(&acct, &AssetType::new("USDC")), 700);
        // A different asset is unaffected.
        assert_eq!(s.available(&acct, &AssetType::new("BTC")), 0);
    }
}
