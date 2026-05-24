//! All mutable shared state lives here. The WS handlers and RFQ orchestrator
//! both work against [`AppState`] — never against raw fields — so any future
//! refactor of how data is stored doesn't ripple out.

pub mod accounts;
pub mod buckets;
pub mod mm_registry;
pub mod reputation;
pub mod reservations;

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, trace, warn};

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::events::{ChainEvent, IndexedEvent};
use shared::protocol_types::ids::ObjectId;

pub use accounts::{AccountMirror, AccountStore};
pub use buckets::{BucketStore, BucketView};
pub use mm_registry::{MmConnection, MmRegistry};
pub use reputation::{ReputationStats, ReputationStore};
pub use reservations::{InsertOutcome, Reservation, ReservationTable};

/// Routed MM response — populated by the MM read task, drained by the RFQ
/// orchestrator's matcher.
#[derive(Debug)]
pub enum MmResponse {
    Quote(ObjectId, shared::protocol_types::messages::MmQuotePayload),
    Decline(ObjectId),
}

#[derive(Default)]
pub struct AppState {
    pub accounts: AccountStore,
    pub buckets: BucketStore,
    pub reservations: ReservationTable,
    pub reputation: ReputationStore,
    pub mms: MmRegistry,
    /// `request_id → matcher_tx`. Populated when an RFQ is broadcast,
    /// removed when the RFQ completes. The MM read task looks up its
    /// `Quote`/`Decline` response's `request_id` here to route it.
    pub pending_rfqs: DashMap<String, mpsc::Sender<MmResponse>>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed an indexer event into the state. Idempotent at the event-id
    /// level: the indexer guarantees one delivery per sequence, but the
    /// store updates are themselves idempotent (re-applying a write to the
    /// same range_start is a no-op since we just set the same totals).
    pub fn ingest_event(&self, indexed: &IndexedEvent) {
        trace!(sequence = indexed.sequence, event = ?std::mem::discriminant(&indexed.event), "ingesting indexer event");
        match &indexed.event {
            ChainEvent::BucketCreated(b) => {
                self.buckets.upsert(
                    b.bucket_id,
                    BucketView {
                        asset_type: b.asset_type.clone(),
                        settlement_type: b.settlement_type.clone(),
                        strike: b.strike,
                        expiry_ms: b.expiry_ms,
                        total_written: 0,
                        exercise_cursor: 0,
                    },
                );
            }
            ChainEvent::WriteExecuted(w) => {
                self.buckets.set_total_written(w.bucket_id, w.range_end);
                // The signer's reservation for this nonce is now satisfied —
                // release if present.
                if let Some(r) = self.reservations.release(w.signer_account_id, w.nonce) {
                    debug!(
                        account = %r.account_id,
                        nonce = r.nonce,
                        "released reservation for executed write"
                    );
                    self.reputation.record_executed(w.signer_account_id);
                }
            }
            ChainEvent::Exercised(e) => {
                self.buckets.set_cursor_only(e.bucket_id, e.cursor_after);
            }
            ChainEvent::Redeemed(_) => {}
            ChainEvent::ExpiredOptionBurned(_) => {}
            ChainEvent::BucketCleaned(_) => {}
            ChainEvent::AccountCreated(a) => {
                self.accounts.set_signing_key(
                    a.account_id,
                    a.signing_scheme,
                    a.signing_pubkey.clone(),
                );
                self.accounts.set_owner(a.account_id, a.owner);
            }
            ChainEvent::AccountDeposit(d) => {
                self.accounts.apply_delta(d.account_id, d.asset_type.clone(), d.amount as i128);
            }
            ChainEvent::AccountWithdraw(w) => {
                self.accounts.apply_delta(
                    w.account_id,
                    w.asset_type.clone(),
                    -(w.amount as i128),
                );
            }
            ChainEvent::SigningKeyRotated(r) => {
                self.accounts.set_signing_key(
                    r.account_id,
                    r.new_scheme,
                    r.new_pubkey.clone(),
                );
            }
            ChainEvent::FeeUpdated(_) | ChainEvent::TreasuryWithdrawn(_) => {}
        }
    }

    pub fn available(&self, account: ObjectId, asset: &AssetType) -> u64 {
        self.accounts.available(&self.reservations, account, asset)
    }
}

impl shared::indexer_client::EventSink for AppState {
    fn ingest_event(&self, event: &IndexedEvent) {
        AppState::ingest_event(self, event);
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
    use shared::protocol_types::asset::AssetType;
    use shared::protocol_types::events::{
        AccountCreated, AccountDeposit, BucketCreated, ChainEvent, IndexedEvent,
    };
    use shared::protocol_types::ids::SuiAddress;

    fn evt(seq: u64, e: ChainEvent) -> IndexedEvent {
        IndexedEvent {
            sequence: seq,
            timestamp_ms: 1,
            event: e,
        }
    }

    #[test]
    fn ingest_populates_balances_and_buckets() {
        let s = AppState::new();
        let account = ObjectId::new([0x01; 32]);
        let bucket = ObjectId::new([0x02; 32]);
        s.ingest_event(&evt(
            0,
            ChainEvent::AccountCreated(AccountCreated {
                account_id: account,
                owner: SuiAddress::ZERO,
                signing_scheme: shared::protocol_types::SigningScheme::Ed25519,
                signing_pubkey: vec![0xab; 32],
            }),
        ));
        s.ingest_event(&evt(
            1,
            ChainEvent::AccountDeposit(AccountDeposit {
                account_id: account,
                asset_type: AssetType::new("USDC"),
                amount: 1_000,
            }),
        ));
        s.ingest_event(&evt(
            2,
            ChainEvent::BucketCreated(BucketCreated {
                bucket_id: bucket,
                asset_type: AssetType::new("BTC"),
                settlement_type: AssetType::new("USDC"),
                expiry_ms: 1_000,
                strike: 50,
            }),
        ));

        assert_eq!(s.available(account, &AssetType::new("USDC")), 1_000);
        let b = s.buckets.get(&bucket).unwrap();
        assert_eq!(b.strike, 50);
        assert_eq!(s.accounts.snapshot(&account).unwrap().signing_pubkey, vec![0xab; 32]);
    }
}
