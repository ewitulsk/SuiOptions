//! Reservation table — the Solana port of the Sui twin's.
//!
//! When an MM signs a quote, the service reserves their balance for that
//! quote until: the quote executes on chain (released by indexer event) or
//! its TTL passes (released by the eviction loop). Reservations are keyed
//! `(account, nonce)` — the same key chain-side uses for replay prevention
//! (the `nonce_record` PDA), so any single quote can map back unambiguously.
//!
//! The math is intentionally minimal:
//!
//! ```text
//! available[account, mint] = on_chain_balance[account, mint]
//!                          - Σ amount over active reservations
//!                               where reservation.mint == mint
//! ```

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tracing::{debug, trace};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// The MM's MmAccount address (base58).
    pub account_id: String,
    pub nonce: u64,
    /// SPL mint (base58) the reserved amount is denominated in.
    pub mint: String,
    pub amount: u64,
    pub valid_until_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct ReservationTable {
    inner: Mutex<BTreeMap<(String, u64), Reservation>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// `(account, nonce)` already had a reservation. Quotes carry unique
    /// nonces by construction, so the right move is to reject the duplicate
    /// quote — never overwrite.
    DuplicateKey,
}

impl ReservationTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn insert(&self, r: Reservation) -> InsertOutcome {
        let mut g = self.inner.lock();
        let key = (r.account_id.clone(), r.nonce);
        if g.contains_key(&key) {
            debug!(account = %r.account_id, nonce = r.nonce, "duplicate reservation rejected");
            return InsertOutcome::DuplicateKey;
        }
        trace!(account = %r.account_id, nonce = r.nonce, amount = r.amount, %r.mint, valid_until_ms = r.valid_until_ms, "inserting reservation");
        g.insert(key, r);
        metrics::gauge!("quoting_active_reservations").set(g.len() as f64);
        InsertOutcome::Inserted
    }

    /// Returns the removed reservation if it was present.
    pub fn release(&self, account_id: &str, nonce: u64) -> Option<Reservation> {
        let mut g = self.inner.lock();
        let removed = g.remove(&(account_id.to_string(), nonce));
        if removed.is_some() {
            trace!(%account_id, nonce, "released reservation");
            metrics::gauge!("quoting_active_reservations").set(g.len() as f64);
        }
        removed
    }

    /// Sum of `amount` over reservations matching `(account, mint)`. The
    /// available-balance computation lives in [`crate::state::AppState`];
    /// this is the component it subtracts.
    pub fn active_amount(&self, account_id: &str, mint: &str) -> u64 {
        let g = self.inner.lock();
        g.values()
            .filter(|r| r.account_id == account_id && r.mint == mint)
            .map(|r| r.amount)
            .fold(0u64, u64::saturating_add)
    }

    /// Drop every reservation whose TTL has passed at `now_ms`. Returns the
    /// dropped reservations so the caller can notify MMs / update accounts.
    pub fn evict_expired(&self, now_ms: u64) -> Vec<Reservation> {
        let mut g = self.inner.lock();
        let expired: Vec<_> = g
            .iter()
            .filter(|(_, r)| now_ms >= r.valid_until_ms)
            .map(|(k, _)| k.clone())
            .collect();
        if !expired.is_empty() {
            debug!(count = expired.len(), now_ms, "evicting expired reservations");
        }
        let evicted: Vec<_> = expired
            .into_iter()
            .filter_map(|k| g.remove(&k))
            .collect();
        if !evicted.is_empty() {
            metrics::counter!("quoting_reservation_evictions_total").increment(evicted.len() as u64);
            metrics::gauge!("quoting_active_reservations").set(g.len() as f64);
        }
        evicted
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACC: &str = "acc111";
    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const TBTC: &str = "So11111111111111111111111111111111111111112";

    fn r(nonce: u64, amount: u64, valid_until_ms: u64) -> Reservation {
        Reservation {
            account_id: ACC.into(),
            nonce,
            mint: USDC.into(),
            amount,
            valid_until_ms,
            created_at_ms: 0,
        }
    }

    #[test]
    fn insert_and_active_amount_per_mint() {
        let t = ReservationTable::new();
        assert_eq!(t.insert(r(1, 100, 1000)), InsertOutcome::Inserted);
        assert_eq!(t.insert(r(2, 250, 1000)), InsertOutcome::Inserted);
        // Different mint is irrelevant.
        let mut other = r(3, 9999, 1000);
        other.mint = TBTC.into();
        t.insert(other);
        assert_eq!(t.active_amount(ACC, USDC), 350);
        assert_eq!(t.active_amount(ACC, TBTC), 9999);
    }

    #[test]
    fn duplicate_nonce_is_rejected() {
        let t = ReservationTable::new();
        assert_eq!(t.insert(r(1, 100, 1000)), InsertOutcome::Inserted);
        assert_eq!(t.insert(r(1, 200, 9999)), InsertOutcome::DuplicateKey);
        // Original survives.
        assert_eq!(t.active_amount(ACC, USDC), 100);
    }

    #[test]
    fn release_returns_reservation() {
        let t = ReservationTable::new();
        t.insert(r(7, 50, 1000));
        let r = t.release(ACC, 7).unwrap();
        assert_eq!(r.amount, 50);
        assert!(t.release(ACC, 7).is_none());
    }

    #[test]
    fn evict_drops_expired_reservations() {
        let t = ReservationTable::new();
        t.insert(r(1, 10, 100));
        t.insert(r(2, 20, 200));
        t.insert(r(3, 30, 300));
        let dropped = t.evict_expired(200);
        // Nonces 1 and 2 are expired (now ≥ valid_until_ms).
        let mut nonces: Vec<_> = dropped.iter().map(|r| r.nonce).collect();
        nonces.sort();
        assert_eq!(nonces, vec![1, 2]);
        assert_eq!(t.len(), 1);
    }
}
