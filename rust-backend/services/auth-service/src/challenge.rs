//! In-memory login-challenge store.
//!
//! `/challenge` mints a single-use, time-boxed message the wallet signs;
//! `/login` consumes it. Holding nonces in process is fine: the set is tiny,
//! short-lived, and a restart just forces clients to request a fresh
//! challenge.

use dashmap::DashMap;
use rand::RngCore;

use crate::jwt::now_secs;

/// The exact human-readable message a client must sign, keyed for lookup.
pub struct ChallengeStore {
    /// message text -> unix-seconds expiry.
    inner: DashMap<String, u64>,
    ttl_secs: u64,
}

impl ChallengeStore {
    pub fn new(ttl_secs: u64) -> Self {
        Self { inner: DashMap::new(), ttl_secs }
    }

    /// Mint a fresh challenge message and remember it until it expires.
    pub fn issue(&self) -> String {
        let mut nonce = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let message = format!(
            "SuiOptions admin login\nnonce: {}",
            hex::encode(nonce)
        );
        self.inner.insert(message.clone(), now_secs() + self.ttl_secs);
        self.prune();
        message
    }

    /// Consume `message`: returns true exactly once for a live challenge,
    /// false if unknown or expired.
    pub fn consume(&self, message: &str) -> bool {
        match self.inner.remove(message) {
            Some((_, exp)) => now_secs() < exp,
            None => false,
        }
    }

    /// Drop expired entries opportunistically.
    fn prune(&self) {
        let now = now_secs();
        self.inner.retain(|_, exp| *exp > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_then_consume_once() {
        let s = ChallengeStore::new(300);
        let m = s.issue();
        assert!(s.consume(&m));
        // second consume fails — single use
        assert!(!s.consume(&m));
    }

    #[test]
    fn unknown_message_rejected() {
        let s = ChallengeStore::new(300);
        assert!(!s.consume("nope"));
    }

    #[test]
    fn expired_rejected() {
        let s = ChallengeStore::new(0);
        let m = s.issue();
        // ttl 0 → expiry == now; consume requires now < exp
        assert!(!s.consume(&m));
    }
}
