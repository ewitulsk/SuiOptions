//! Off-chain Account mirror.
//!
//! Stores the on-chain balances last reported by the indexer plus the
//! account's signing pubkey. `available(asset)` subtracts the
//! [`ReservationTable`]'s active reservations — that's the figure quotes
//! get validated against, so an oversigning MM is caught here before a
//! transaction ever gets built.

use std::collections::BTreeMap;

use dashmap::DashMap;
use parking_lot::RwLock;
use tracing::{debug, trace};

use shared::protocol_types::asset::AssetType;
use shared::protocol_types::ids::{ObjectId, SuiAddress};
use shared::protocol_types::SigningScheme;

use super::reservations::ReservationTable;

#[derive(Clone, Debug, Default)]
pub struct AccountMirror {
    pub owner: Option<SuiAddress>,
    /// Tag the indexer observed in the most recent `AccountCreated` /
    /// `SigningKeyRotated` event. `None` means the indexer hasn't seen one
    /// yet (boot race) — quote/auth verification must fail closed in that
    /// case.
    pub signing_scheme: Option<SigningScheme>,
    pub signing_pubkey: Vec<u8>,
    pub balances: BTreeMap<AssetType, u64>,
}

#[derive(Default)]
pub struct AccountStore {
    accounts: DashMap<ObjectId, RwLock<AccountMirror>>,
}

impl AccountStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the stored mirror — used when a fresh snapshot or
    /// `AccountCreated` arrives.
    pub fn upsert(&self, id: ObjectId, mirror: AccountMirror) {
        debug!(%id, scheme = ?mirror.signing_scheme, balances = mirror.balances.len(), "upserting account mirror");
        self.accounts.insert(id, RwLock::new(mirror));
    }

    /// Apply a balance delta from a `AccountDeposit` / `AccountWithdraw`
    /// event. Creates the account row if absent so we don't drop balances
    /// while we're still catching up on `AccountCreated`.
    pub fn apply_delta(&self, id: ObjectId, asset: AssetType, signed_amount: i128) {
        let entry = self
            .accounts
            .entry(id)
            .or_insert_with(|| RwLock::new(AccountMirror::default()));
        let mut g = entry.write();
        let cur = *g.balances.get(&asset).unwrap_or(&0) as i128;
        let next = (cur + signed_amount).clamp(0, u64::MAX as i128) as u64;
        trace!(%id, %asset, signed_amount, prev = cur, next, "applying balance delta");
        g.balances.insert(asset, next);
    }

    /// Set the registered signing scheme + pubkey for an Account.
    /// `AccountCreated` and `SigningKeyRotated` both go through here so
    /// they can't drift.
    pub fn set_signing_key(&self, id: ObjectId, scheme: SigningScheme, key: Vec<u8>) {
        debug!(%id, ?scheme, pubkey_len = key.len(), "setting signing key");
        let entry = self
            .accounts
            .entry(id)
            .or_insert_with(|| RwLock::new(AccountMirror::default()));
        let mut w = entry.write();
        w.signing_scheme = Some(scheme);
        w.signing_pubkey = key;
    }

    pub fn set_owner(&self, id: ObjectId, owner: SuiAddress) {
        let entry = self
            .accounts
            .entry(id)
            .or_insert_with(|| RwLock::new(AccountMirror::default()));
        entry.write().owner = Some(owner);
    }

    pub fn snapshot(&self, id: &ObjectId) -> Option<AccountMirror> {
        self.accounts.get(id).map(|e| e.read().clone())
    }

    pub fn balance(&self, id: &ObjectId, asset: &AssetType) -> u64 {
        self.accounts
            .get(id)
            .map(|e| *e.read().balances.get(asset).unwrap_or(&0))
            .unwrap_or(0)
    }

    /// `available = balance - active_reservations_in_same_asset`. Saturates
    /// at zero so a momentarily-stale view (balance not yet reflecting an
    /// in-flight execution) can't read negative.
    pub fn available(
        &self,
        reservations: &ReservationTable,
        id: ObjectId,
        asset: &AssetType,
    ) -> u64 {
        let bal = self.balance(&id, asset);
        let active = reservations.active_amount(id, asset);
        bal.saturating_sub(active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_round_trip() {
        let store = AccountStore::new();
        let id = ObjectId::new([0x01; 32]);
        store.apply_delta(id, AssetType::new("USDC"), 1_000);
        store.apply_delta(id, AssetType::new("USDC"), -250);
        store.apply_delta(id, AssetType::new("BTC"), 50);

        assert_eq!(store.balance(&id, &AssetType::new("USDC")), 750);
        assert_eq!(store.balance(&id, &AssetType::new("BTC")), 50);
    }

    #[test]
    fn available_subtracts_reservations_per_asset() {
        let store = AccountStore::new();
        let res = ReservationTable::new();
        let id = ObjectId::new([0x01; 32]);
        store.apply_delta(id, AssetType::new("USDC"), 1_000);
        res.insert(super::super::reservations::Reservation {
            account_id: id,
            nonce: 1,
            asset_type: AssetType::new("USDC"),
            amount: 300,
            valid_until_ms: 9999,
            created_at_ms: 0,
        });

        assert_eq!(store.available(&res, id, &AssetType::new("USDC")), 700);
        // No reservations in BTC → full balance.
        assert_eq!(store.available(&res, id, &AssetType::new("BTC")), 0);
    }

    #[test]
    fn available_saturates_at_zero() {
        let store = AccountStore::new();
        let res = ReservationTable::new();
        let id = ObjectId::new([0x01; 32]);
        store.apply_delta(id, AssetType::new("USDC"), 100);
        res.insert(super::super::reservations::Reservation {
            account_id: id,
            nonce: 1,
            asset_type: AssetType::new("USDC"),
            amount: 500,
            valid_until_ms: 9999,
            created_at_ms: 0,
        });
        assert_eq!(store.available(&res, id, &AssetType::new("USDC")), 0);
    }
}
