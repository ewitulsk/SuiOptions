use bridge_signer::ThresholdSigner;
use bridge_types::Bytes32;

use crate::ratelimit::RateLimiter;
use crate::sessions::SessionStore;
use crate::verifier::SourceVerifier;

/// Shared signer state. At M1 the keys live in process memory loaded from config;
/// at M3 they become Seal-provisioned shares loaded in-enclave.
pub struct AppState {
    pub signer: ThresholdSigner,
    pub verifier: Box<dyn SourceVerifier>,
    /// Digest domain separator (spec §2.2), derived from the deployment salt.
    pub domain_sep: Bytes32,
    /// Group-key id the envelope references for Sui-destined (Ed25519) messages.
    pub ed25519_group_pubkey_id: u32,
    /// Group-key id for EVM-destined (ECDSA) messages.
    pub ecdsa_group_pubkey_id: u32,
    /// Signing sessions, keyed by message hash (§5.3 async surface).
    pub sessions: SessionStore,
    /// Per-IP limiter for `POST /sign_requests`.
    pub rate_limiter: RateLimiter,
}
