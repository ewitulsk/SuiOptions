//! Connected-MM registry. Live for the lifetime of one WS connection.
//!
//! Storing the outbound mpsc sender lets the RFQ broadcast a single
//! `RFQBroadcast` to every interested MM in one call without holding any WS
//! sink — the per-connection task drains the channel.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use shared::protocol_types::ids::ObjectId;
use shared::protocol_types::messages::ServiceToMm;
use shared::protocol_types::sides::MmRole;

#[derive(Clone)]
pub struct MmConnection {
    pub account_id: ObjectId,
    pub roles: Arc<RwLock<Vec<MmRole>>>,
    pub tx: mpsc::Sender<ServiceToMm>,
}

impl MmConnection {
    pub fn serves(&self, role: MmRole) -> bool {
        self.roles.read().contains(&role)
    }
}

#[derive(Default)]
pub struct MmRegistry {
    by_account: DashMap<ObjectId, MmConnection>,
}

impl MmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, conn: MmConnection) {
        self.by_account.insert(conn.account_id, conn);
    }

    pub fn remove(&self, account_id: &ObjectId) {
        self.by_account.remove(account_id);
    }

    pub fn get(&self, account_id: &ObjectId) -> Option<MmConnection> {
        self.by_account.get(account_id).map(|e| e.clone())
    }

    pub fn all_for_role(&self, role: MmRole) -> Vec<MmConnection> {
        self.by_account
            .iter()
            .filter(|e| e.value().serves(role))
            .map(|e| e.value().clone())
            .collect()
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
            account_id: ObjectId::new([0x01; 32]),
            roles: Arc::new(RwLock::new(vec![MmRole::TraderMm])),
            tx: tx_a,
        });
        r.insert(MmConnection {
            account_id: ObjectId::new([0x02; 32]),
            roles: Arc::new(RwLock::new(vec![MmRole::WriterMm, MmRole::TraderMm])),
            tx: tx_b,
        });

        assert_eq!(r.all_for_role(MmRole::TraderMm).len(), 2);
        assert_eq!(r.all_for_role(MmRole::WriterMm).len(), 1);
    }
}
