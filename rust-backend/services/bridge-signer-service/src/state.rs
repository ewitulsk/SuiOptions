use bridge_signer::ThresholdSigner;

use crate::verifier::SourceVerifier;

/// Shared, immutable signer state. At M1 the keys live in process memory loaded
/// from config; at M3 they become Seal-provisioned shares loaded in-enclave.
pub struct AppState {
    pub signer: ThresholdSigner,
    pub verifier: Box<dyn SourceVerifier>,
    /// Group-key id the envelope references for Sui-destined (Ed25519) messages.
    pub ed25519_group_pubkey_id: u32,
    /// Group-key id for EVM-destined (ECDSA) messages.
    pub ecdsa_group_pubkey_id: u32,
}
