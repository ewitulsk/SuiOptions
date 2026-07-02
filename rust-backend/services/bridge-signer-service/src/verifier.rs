//! The §5.4 security boundary. Before signing, the node confirms the message was
//! committed by the *registered* source Outbox AND that the commitment is final
//! — the narrow, auditable check that makes the signer safe (not free-form event
//! scraping). Anything the source can't prove committed-at-finality is refused.
//!
//! Structure:
//! - [`SourceVerifier`] — the boundary the handler calls before signing.
//! - [`CommitmentProbe`] — one source-chain RPC provider's view of "is this
//!   digest committed at finality?". Mockable, so the quorum logic is unit-tested
//!   without a network; concrete EVM/Sui probes live in [`crate::probe`].
//! - [`RpcVerifier`] — per source chain, requires **every configured provider**
//!   to independently confirm the commitment (spec §5.4: ≥2 independent RPCs).
//!   A single provider is allowed only with an explicit opt-in.

use std::collections::HashMap;

use anyhow::{bail, Result};
use async_trait::async_trait;
use bridge_types::{chain_id, Bytes32, CrossChainMessage};
use thiserror::Error;
use tracing::warn;

use crate::config::SourceChainConfig;

#[derive(Debug, Error)]
pub enum VerifyError {
    /// No source route is configured for the message's `src_chain_id`.
    #[error("no configured source route for src_chain_id={0}")]
    UnknownRoute(u32),
    /// The Outbox has not committed this exact message (or not yet at finality).
    #[error("source Outbox has not committed message (src_chain={src_chain_id}, nonce={nonce}) at finality")]
    NotCommitted { src_chain_id: u32, nonce: u64 },
    /// A provider was unreachable, or providers disagreed — fail closed.
    #[error("source verification unavailable: {0}")]
    Unavailable(String),
}

/// One source-chain provider's answer for a specific digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commitment {
    /// The registered Outbox committed this digest and it is past finality.
    Final,
    /// Committed, but not yet at the required confirmation depth.
    Pending,
    /// No matching commitment found on this provider.
    NotFound,
}

/// A single RPC provider's view of a source chain's Outbox commitments.
#[async_trait]
pub trait CommitmentProbe: Send + Sync {
    /// Short label (host) for logs.
    fn label(&self) -> &str;
    /// Does this provider see `digest` committed by the registered Outbox at
    /// finality? Errors are transport/availability failures (fail closed).
    async fn check(&self, digest: &Bytes32) -> Result<Commitment>;
}

#[async_trait]
pub trait SourceVerifier: Send + Sync {
    async fn verify_committed(&self, message: &CrossChainMessage) -> Result<(), VerifyError>;
}

/// DEV ONLY. Skips the source-commitment check entirely — signs any well-formed
/// message. The trust model collapses without §5.4, so `build` refuses to
/// construct this outside `environment = "dev"`.
pub struct TrustAllVerifier;

#[async_trait]
impl SourceVerifier for TrustAllVerifier {
    async fn verify_committed(&self, _message: &CrossChainMessage) -> Result<(), VerifyError> {
        Ok(())
    }
}

/// Per-source-chain set of probes. Every probe must independently confirm the
/// commitment (quorum = all configured providers).
struct SourceRoute {
    probes: Vec<Box<dyn CommitmentProbe>>,
}

/// The real §5.4 verifier: recompute the message's domain-separated digest and
/// require every configured RPC provider for its source chain to confirm the
/// registered Outbox committed it at finality.
pub struct RpcVerifier {
    domain_sep: Bytes32,
    sources: HashMap<u32, SourceRoute>,
}

impl RpcVerifier {
    /// Assemble from the signer config's `[[source_chains]]`. Validates the
    /// provider count (≥2 unless `allow_single_provider`) and family wiring.
    pub fn from_config(domain_sep: Bytes32, source_chains: &[SourceChainConfig]) -> Result<Self> {
        let mut sources = HashMap::new();
        for sc in source_chains {
            let probes = crate::probe::build_probes(sc)?;
            if probes.len() < 2 && !sc.allow_single_provider {
                bail!(
                    "source chain {} has {} RPC provider(s); §5.4 wants ≥2 (set allow_single_provider=true to override)",
                    sc.internal_chain_id,
                    probes.len()
                );
            }
            if sources.insert(sc.internal_chain_id, SourceRoute { probes }).is_some() {
                bail!("duplicate source chain config for internal_chain_id={}", sc.internal_chain_id);
            }
        }
        Ok(Self { domain_sep, sources })
    }

    #[cfg(test)]
    fn from_routes(domain_sep: Bytes32, routes: HashMap<u32, Vec<Box<dyn CommitmentProbe>>>) -> Self {
        Self {
            domain_sep,
            sources: routes.into_iter().map(|(k, probes)| (k, SourceRoute { probes })).collect(),
        }
    }
}

#[async_trait]
impl SourceVerifier for RpcVerifier {
    async fn verify_committed(&self, message: &CrossChainMessage) -> Result<(), VerifyError> {
        let route = self
            .sources
            .get(&message.src_chain_id)
            .ok_or(VerifyError::UnknownRoute(message.src_chain_id))?;

        let digest = message.digest(&self.domain_sep);
        let not_committed =
            VerifyError::NotCommitted { src_chain_id: message.src_chain_id, nonce: message.nonce };

        // Every provider must independently return Final. Any Pending/NotFound is
        // a refusal (fail closed); any transport error is Unavailable. A split
        // vote (one Final, one NotFound) therefore refuses — safe by construction.
        for probe in &route.probes {
            match probe.check(&digest).await {
                Ok(Commitment::Final) => {}
                Ok(Commitment::Pending) | Ok(Commitment::NotFound) => return Err(not_committed),
                Err(e) => {
                    return Err(VerifyError::Unavailable(format!("{}: {e:#}", probe.label())))
                }
            }
        }
        Ok(())
    }
}

/// Construct the configured verifier. In `environment = "dev"`, `trust_all` is
/// allowed (with a loud warning); anywhere else it is a hard error. `rpc` builds
/// an [`RpcVerifier`] from `[[source_chains]]`.
pub fn build(
    mode: &str,
    environment: &str,
    domain_sep: Bytes32,
    source_chains: &[SourceChainConfig],
) -> Result<Box<dyn SourceVerifier>> {
    match mode {
        "trust_all" => {
            if environment != "dev" {
                bail!(
                    "source_verifier=trust_all is DEV ONLY (environment={environment}); \
                     it disables the §5.4 boundary and lets the signer sign anything"
                );
            }
            warn!(
                "source_verifier=trust_all — DEV ONLY, §5.4 source-commitment check is SKIPPED"
            );
            Ok(Box::new(TrustAllVerifier))
        }
        "rpc" => {
            if source_chains.is_empty() {
                bail!("source_verifier=rpc requires at least one [[source_chains]] entry");
            }
            Ok(Box::new(RpcVerifier::from_config(domain_sep, source_chains)?))
        }
        other => bail!("unknown source_verifier mode: {other}"),
    }
}

/// True if `internal_chain_id`'s family is one this node can verify.
pub fn is_supported_family(internal_chain_id: u32) -> bool {
    let f = chain_id::family(internal_chain_id);
    f == chain_id::FAMILY_EVM || f == chain_id::FAMILY_SUI
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_types::message::derive_domain_sep;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const SALT: Bytes32 = [0x01; 32];
    const SUI_ID: u32 = 134_217_728;
    const HYPER_ID: u32 = 268_436_454;

    /// Mock probe returning a fixed verdict (or an error), counting calls.
    struct MockProbe {
        name: String,
        verdict: Result<Commitment, ()>,
        calls: Arc<AtomicUsize>,
    }
    impl MockProbe {
        fn new(name: &str, verdict: Result<Commitment, ()>) -> (Box<dyn CommitmentProbe>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Box::new(MockProbe { name: name.into(), verdict, calls: calls.clone() }),
                calls,
            )
        }
    }
    #[async_trait]
    impl CommitmentProbe for MockProbe {
        fn label(&self) -> &str {
            &self.name
        }
        async fn check(&self, _digest: &Bytes32) -> Result<Commitment> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict.map_err(|_| anyhow::anyhow!("mock transport error"))
        }
    }

    fn msg(src: u32) -> CrossChainMessage {
        CrossChainMessage::new(src, SUI_ID, 7, [0xab; 32], [0xcd; 32], b"hello-bridge".to_vec())
    }

    fn verifier(src: u32, probes: Vec<Box<dyn CommitmentProbe>>) -> RpcVerifier {
        let mut routes: HashMap<u32, Vec<Box<dyn CommitmentProbe>>> = HashMap::new();
        routes.insert(src, probes);
        RpcVerifier::from_routes(derive_domain_sep(&SALT), routes)
    }

    #[tokio::test]
    async fn signs_when_all_providers_confirm_final() {
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Final));
        let (p2, _) = MockProbe::new("b", Ok(Commitment::Final));
        let v = verifier(HYPER_ID, vec![p1, p2]);
        assert!(v.verify_committed(&msg(HYPER_ID)).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_unknown_route() {
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Final));
        let v = verifier(HYPER_ID, vec![p1]);
        // message from SUI_ID, but only HYPER_ID is configured.
        let err = v.verify_committed(&msg(SUI_ID)).await.unwrap_err();
        assert!(matches!(err, VerifyError::UnknownRoute(SUI_ID)));
    }

    #[tokio::test]
    async fn rejects_when_a_provider_reports_not_found() {
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Final));
        let (p2, _) = MockProbe::new("b", Ok(Commitment::NotFound));
        let v = verifier(HYPER_ID, vec![p1, p2]);
        let err = v.verify_committed(&msg(HYPER_ID)).await.unwrap_err();
        assert!(matches!(err, VerifyError::NotCommitted { .. }));
    }

    #[tokio::test]
    async fn rejects_when_commitment_not_final() {
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Pending));
        let v = verifier(HYPER_ID, vec![p1]);
        let err = v.verify_committed(&msg(HYPER_ID)).await.unwrap_err();
        assert!(matches!(err, VerifyError::NotCommitted { .. }));
    }

    #[tokio::test]
    async fn provider_disagreement_refuses() {
        // one Final, one NotFound → refuse (fail closed), don't sign.
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Final));
        let (p2, _) = MockProbe::new("b", Ok(Commitment::NotFound));
        let v = verifier(HYPER_ID, vec![p1, p2]);
        assert!(v.verify_committed(&msg(HYPER_ID)).await.is_err());
    }

    #[tokio::test]
    async fn transport_error_is_unavailable() {
        let (p1, _) = MockProbe::new("a", Ok(Commitment::Final));
        let (p2, _) = MockProbe::new("b", Err(()));
        let v = verifier(HYPER_ID, vec![p1, p2]);
        let err = v.verify_committed(&msg(HYPER_ID)).await.unwrap_err();
        assert!(matches!(err, VerifyError::Unavailable(_)));
    }

    #[tokio::test]
    async fn build_refuses_trust_all_outside_dev() {
        assert!(build("trust_all", "prod", derive_domain_sep(&SALT), &[]).is_err());
        assert!(build("trust_all", "dev", derive_domain_sep(&SALT), &[]).is_ok());
    }

    #[tokio::test]
    async fn build_rpc_requires_source_chains() {
        assert!(build("rpc", "prod", derive_domain_sep(&SALT), &[]).is_err());
    }
}
