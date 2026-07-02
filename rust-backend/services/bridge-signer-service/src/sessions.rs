//! In-memory signing-session store (bridge-spec.md §5.3). Keyed by the 32-byte
//! message hash so a session is created **at most once per digest** — duplicate
//! `POST /sign_requests` for the same message coalesce onto the existing session
//! instead of re-verifying and re-signing. At M1 a session completes inline
//! (verify → sign) before the POST returns; the async surface is what M3's
//! multi-round MPC swaps its internals under without changing the interface.
//!
//! Terminal (`Signed`) sessions are TTL-evicted; the map is bounded to cap the
//! DoS surface. Verify failures are NOT cached — they 422 at the door and the
//! session is abandoned so a later retry (once the source reaches finality) can
//! re-admit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bridge_types::SignatureEnvelope;

/// The state of a signing session, as returned to pollers.
#[derive(Clone, Debug)]
pub enum SignStatus {
    /// Admitted; signing in progress (trivially fast at M1).
    Pending,
    /// Completed — the aggregated signature envelope is ready.
    Signed(Box<SignatureEnvelope>),
}

struct Entry {
    status: SignStatus,
    /// When this entry last became its current state (for TTL eviction).
    stamped: Instant,
}

/// Result of trying to claim a session for a message hash.
pub enum Admit {
    /// A session already exists; here is its current status (coalesced).
    Existing(SignStatus),
    /// Freshly claimed — the caller owns it and must `finalize` or `abandon`.
    New,
    /// The bounded map is full; shed load.
    Full,
}

pub struct SessionStore {
    inner: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    cap: usize,
}

impl SessionStore {
    pub fn new(ttl_secs: u64, cap: usize) -> Self {
        Self { inner: Mutex::new(HashMap::new()), ttl: Duration::from_secs(ttl_secs), cap }
    }

    /// Evict terminal sessions older than the TTL. Pending sessions are kept
    /// (they're in-flight). Caller must hold the lock.
    fn evict(&self, map: &mut HashMap<String, Entry>) {
        let ttl = self.ttl;
        map.retain(|_, e| matches!(e.status, SignStatus::Pending) || e.stamped.elapsed() < ttl);
    }

    /// Claim a session for `hash`, or coalesce onto an existing one.
    pub fn admit(&self, hash: &str) -> Admit {
        let mut map = self.inner.lock().unwrap();
        self.evict(&mut map);
        if let Some(e) = map.get(hash) {
            return Admit::Existing(e.status.clone());
        }
        if map.len() >= self.cap {
            return Admit::Full;
        }
        map.insert(hash.to_string(), Entry { status: SignStatus::Pending, stamped: Instant::now() });
        Admit::New
    }

    /// Mark a claimed session complete with its signature.
    pub fn finalize(&self, hash: &str, envelope: SignatureEnvelope) {
        let mut map = self.inner.lock().unwrap();
        if let Some(e) = map.get_mut(hash) {
            e.status = SignStatus::Signed(Box::new(envelope));
            e.stamped = Instant::now();
        }
    }

    /// Drop a claimed session (verify failed / signing error) so it can retry.
    pub fn abandon(&self, hash: &str) {
        self.inner.lock().unwrap().remove(hash);
    }

    /// Current status of a session, if any.
    pub fn get(&self, hash: &str) -> Option<SignStatus> {
        let mut map = self.inner.lock().unwrap();
        self.evict(&mut map);
        map.get(hash).map(|e| e.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_types::envelope::SCHEME_ED25519;

    fn env() -> SignatureEnvelope {
        SignatureEnvelope { scheme_tag: SCHEME_ED25519, group_pubkey_id: 1, signature: vec![1, 2, 3] }
    }

    #[test]
    fn admit_is_idempotent_per_hash() {
        let s = SessionStore::new(60, 100);
        assert!(matches!(s.admit("0xaa"), Admit::New));
        // second admit coalesces onto the pending session
        assert!(matches!(s.admit("0xaa"), Admit::Existing(SignStatus::Pending)));
        s.finalize("0xaa", env());
        assert!(matches!(s.admit("0xaa"), Admit::Existing(SignStatus::Signed(_))));
    }

    #[test]
    fn abandon_allows_readmit() {
        let s = SessionStore::new(60, 100);
        assert!(matches!(s.admit("0xbb"), Admit::New));
        s.abandon("0xbb");
        assert!(s.get("0xbb").is_none());
        assert!(matches!(s.admit("0xbb"), Admit::New)); // retryable
    }

    #[test]
    fn bounded_map_sheds_load() {
        let s = SessionStore::new(60, 1);
        assert!(matches!(s.admit("0x1"), Admit::New));
        assert!(matches!(s.admit("0x2"), Admit::Full)); // cap reached
    }

    #[test]
    fn terminal_sessions_evict_after_ttl() {
        let s = SessionStore::new(0, 100); // ttl 0 → signed entries expire immediately
        s.admit("0xcc");
        s.finalize("0xcc", env());
        // a Signed entry with elapsed() >= 0 ttl is evicted on next access
        assert!(s.get("0xcc").is_none());
    }

    #[test]
    fn pending_survives_eviction() {
        let s = SessionStore::new(0, 100);
        s.admit("0xdd"); // Pending, ttl 0
        assert!(matches!(s.get("0xdd"), Some(SignStatus::Pending)));
    }
}
