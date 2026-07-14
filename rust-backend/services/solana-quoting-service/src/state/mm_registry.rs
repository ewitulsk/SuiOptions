//! Connected-MM registry. Live for the lifetime of one WS connection.
//!
//! Storing the outbound mpsc sender lets the RFQ broadcast a single
//! `RFQBroadcast` to every interested MM in one call without holding any WS
//! sink — the per-connection task drains the channel.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, info};

use protocol_types::sides::MmRole;

use crate::messages::ServiceToMm;

#[derive(Clone)]
pub struct MmConnection {
    /// The MM's MmAccount address (base58).
    pub account_id: String,
    pub roles: Arc<RwLock<Vec<MmRole>>>,
    /// Whether this MM opted in to unsigned bulk-view RFQs (Hello.bulk_view).
    pub bulk_view: bool,
    pub tx: mpsc::Sender<ServiceToMm>,
}

impl MmConnection {
    pub fn serves(&self, role: MmRole) -> bool {
        self.roles.read().contains(&role)
    }
}

#[derive(Default)]
pub struct MmRegistry {
    by_account: DashMap<String, MmConnection>,
}

impl MmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, conn: MmConnection) {
        info!(account = %conn.account_id, "mm registered");
        self.by_account.insert(conn.account_id.clone(), conn);
    }

    pub fn remove(&self, account_id: &str) {
        info!(%account_id, "mm disconnected");
        self.by_account.remove(account_id);
    }

    pub fn get(&self, account_id: &str) -> Option<MmConnection> {
        self.by_account.get(account_id).map(|e| e.clone())
    }

    pub fn all_for_role(&self, role: MmRole) -> Vec<MmConnection> {
        let conns: Vec<_> = self.by_account
            .iter()
            .filter(|e| e.value().serves(role))
            .map(|e| e.value().clone())
            .collect();
        debug!(?role, count = conns.len(), "queried mms for role");
        conns
    }

    /// MMs that serve `role` AND opted in to bulk-view RFQs. The bulk-view
    /// orchestrator broadcasts only to these.
    pub fn all_for_bulk_view(&self, role: MmRole) -> Vec<MmConnection> {
        let conns: Vec<_> = self.by_account
            .iter()
            .filter(|e| e.value().serves(role) && e.value().bulk_view)
            .map(|e| e.value().clone())
            .collect();
        debug!(?role, count = conns.len(), "queried bulk-view mms for role");
        conns
    }

    pub fn len(&self) -> usize {
        self.by_account.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn filter_by_role() {
        let r = MmRegistry::new();
        let (tx_a, _) = mpsc::channel(1);
        let (tx_b, _) = mpsc::channel(1);
        r.insert(MmConnection {
            account_id: "acc-a".into(),
            roles: Arc::new(RwLock::new(vec![MmRole::TraderMm])),
            bulk_view: false,
            tx: tx_a,
        });
        r.insert(MmConnection {
            account_id: "acc-b".into(),
            roles: Arc::new(RwLock::new(vec![MmRole::WriterMm, MmRole::TraderMm])),
            bulk_view: true,
            tx: tx_b,
        });

        assert_eq!(r.all_for_role(MmRole::TraderMm).len(), 2);
        assert_eq!(r.all_for_role(MmRole::WriterMm).len(), 1);
        // Only the second MM opted in to bulk-view.
        assert_eq!(r.all_for_bulk_view(MmRole::TraderMm).len(), 1);
        assert_eq!(r.all_for_bulk_view(MmRole::WriterMm).len(), 1);
    }
}
