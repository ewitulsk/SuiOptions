//! The §5.4 security boundary. Before signing, the node must confirm the
//! message was committed by the registered Outbox on its source chain AND that
//! the commitment is final. This is the narrow, auditable check that makes the
//! signer safe — not free-form event scraping.
//!
//! The check is behind a trait so it can be mocked in tests and, later, backed
//! by a trusted full-node/RPC view per source chain. At M1 only the dev
//! `TrustAllVerifier` exists; the RPC implementation is M1's remaining work.

use anyhow::{bail, Result};
use async_trait::async_trait;
use bridge_types::CrossChainMessage;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VerifyError {
    /// The Outbox has not committed this exact message (or not yet at finality).
    #[error("source Outbox has not committed message (src_chain={src_chain_id}, nonce={nonce}) at finality")]
    NotCommitted { src_chain_id: u32, nonce: u64 },
    /// The verifier could not reach/trust its source view.
    #[error("source verification unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait SourceVerifier: Send + Sync {
    async fn verify_committed(&self, message: &CrossChainMessage) -> Result<(), VerifyError>;
}

/// DEV ONLY. Skips the source-commitment check entirely — signs any well-formed
/// message. The trust model collapses without §5.4, so this must never run in a
/// real deployment.
pub struct TrustAllVerifier;

#[async_trait]
impl SourceVerifier for TrustAllVerifier {
    async fn verify_committed(&self, _message: &CrossChainMessage) -> Result<(), VerifyError> {
        Ok(())
    }
}

/// Construct the configured verifier. `rpc` is reserved for the on-chain
/// Outbox-commitment + finality check and is not built at M1.
pub fn build(mode: &str) -> Result<Box<dyn SourceVerifier>> {
    match mode {
        "trust_all" => {
            tracing::warn!(
                "source_verifier=trust_all — DEV ONLY, §5.4 source-commitment check is SKIPPED"
            );
            Ok(Box::new(TrustAllVerifier))
        }
        "rpc" => bail!("source_verifier=rpc is not implemented at M1 (needs source-chain RPC view)"),
        other => bail!("unknown source_verifier mode: {other}"),
    }
}
