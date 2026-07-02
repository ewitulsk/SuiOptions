//! Relay orchestration. For each committed source message: skip if the
//! destination already delivered it (gas saver — the on-chain `consumed` set is
//! the real guard), else fetch a signature and submit. The relayer is
//! untrusted; correctness never depends on it.

use anyhow::{bail, Result};
use async_trait::async_trait;
use bridge_types::{chain_id, Bytes32, CrossChainMessage, SignatureEnvelope};
use tracing::{info, warn};

use crate::signer_client::RemoteSigner;

/// Yields newly-committed messages from a source chain's Outbox.
#[async_trait]
pub trait SourceWatcher: Send {
    async fn poll(&mut self) -> Result<Vec<CrossChainMessage>>;
}

/// Submits a verified message to a destination chain's Inbox.
#[async_trait]
pub trait DestSubmitter: Send + Sync {
    /// Whether the destination Inbox has already consumed this digest.
    async fn is_delivered(&self, digest: &Bytes32) -> Result<bool>;
    async fn submit(&self, message: &CrossChainMessage, envelope: &SignatureEnvelope)
        -> Result<()>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayOutcome {
    Delivered,
    AlreadyDelivered,
}

/// Routes a message to the submitter for its destination chain family. One
/// relayer process relays every direction: EVM destinations via the generic EVM
/// submitter, Sui destinations via the type-derived Sui submitter.
#[derive(Default)]
pub struct Router {
    evm: Option<Box<dyn DestSubmitter>>,
    sui: Option<Box<dyn DestSubmitter>>,
}

impl Router {
    pub fn with_evm(mut self, s: Box<dyn DestSubmitter>) -> Self {
        self.evm = Some(s);
        self
    }
    pub fn with_sui(mut self, s: Box<dyn DestSubmitter>) -> Self {
        self.sui = Some(s);
        self
    }

    /// Pick the submitter for a message's destination, by family.
    pub fn for_dst(&self, dst_chain_id: u32) -> Result<&dyn DestSubmitter> {
        let family = chain_id::family(dst_chain_id);
        let slot = match family {
            chain_id::FAMILY_EVM => &self.evm,
            chain_id::FAMILY_SUI => &self.sui,
            other => bail!("no submitter configured for destination family {other}"),
        };
        slot.as_deref()
            .ok_or_else(|| anyhow::anyhow!("no submitter configured for destination family {family}"))
    }
}

pub async fn relay_message(
    message: &CrossChainMessage,
    domain_sep: &Bytes32,
    signer: &dyn RemoteSigner,
    submitter: &dyn DestSubmitter,
) -> Result<RelayOutcome> {
    let digest = message.digest(domain_sep);
    if submitter.is_delivered(&digest).await? {
        return Ok(RelayOutcome::AlreadyDelivered);
    }
    let envelope = signer.sign(message).await?;
    submitter.submit(message, &envelope).await?;
    Ok(RelayOutcome::Delivered)
}

/// One poll → relay pass over a source watcher, routing each message to the
/// submitter for its destination family. Returns the number newly delivered. A
/// single message failing to relay is logged and skipped, not fatal.
pub async fn relay_once(
    watcher: &mut dyn SourceWatcher,
    domain_sep: &Bytes32,
    signer: &dyn RemoteSigner,
    router: &Router,
) -> Result<usize> {
    let mut delivered = 0;
    for message in watcher.poll().await? {
        let digest = hex::encode(message.digest(domain_sep));
        let submitter = match router.for_dst(message.dst_chain_id) {
            Ok(s) => s,
            Err(e) => {
                warn!(digest = %digest, dst = message.dst_chain_id, error = %e, "no route, skipping");
                continue;
            }
        };
        match relay_message(&message, domain_sep, signer, submitter).await {
            Ok(RelayOutcome::Delivered) => {
                info!(digest = %digest, nonce = message.nonce, "relayed");
                delivered += 1;
            }
            Ok(RelayOutcome::AlreadyDelivered) => {
                info!(digest = %digest, "already delivered, skipping");
            }
            // Tx-submission failures carry an alert_id per .claude/tx-alerting.md.
            // Losing a delivery race (already consumed on arrival) is benign and
            // surfaces as AlreadyDelivered above, not here.
            Err(e) => tracing::error!(
                alert_id = "tx-failed-bridge-relay",
                digest = %digest,
                dst = message.dst_chain_id,
                error = %format!("{e:#}"),
                "relay submission failed, will retry next poll"
            ),
        }
    }
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    use bridge_signer::ThresholdSigner;
    use bridge_types::chain_id;
    use bridge_types::message::derive_domain_sep;

    const TEST_SALT: Bytes32 = [0x01; 32];

    struct LocalSigner {
        signer: ThresholdSigner,
        domain_sep: Bytes32,
    }
    #[async_trait]
    impl RemoteSigner for LocalSigner {
        async fn sign(&self, m: &CrossChainMessage) -> Result<SignatureEnvelope> {
            Ok(self.signer.sign(m, &self.domain_sep, 1)?)
        }
    }

    struct RecordingSubmitter {
        domain_sep: Bytes32,
        delivered: Mutex<HashSet<Bytes32>>,
        submissions: Mutex<Vec<(CrossChainMessage, SignatureEnvelope)>>,
    }
    #[async_trait]
    impl DestSubmitter for RecordingSubmitter {
        async fn is_delivered(&self, digest: &Bytes32) -> Result<bool> {
            Ok(self.delivered.lock().unwrap().contains(digest))
        }
        async fn submit(&self, m: &CrossChainMessage, e: &SignatureEnvelope) -> Result<()> {
            self.delivered.lock().unwrap().insert(m.digest(&self.domain_sep));
            self.submissions.lock().unwrap().push((m.clone(), e.clone()));
            Ok(())
        }
    }

    fn vector_message() -> CrossChainMessage {
        CrossChainMessage::new(
            chain_id::encode(chain_id::FAMILY_EVM, 998).unwrap(),
            chain_id::encode(chain_id::FAMILY_SUI, 0).unwrap(),
            7,
            [0xab; 32],
            [0xcd; 32],
            b"hello-bridge".to_vec(),
        )
    }

    const SUI_ID: u32 = 134_217_728;
    const HYPER_ID: u32 = 268_436_454;

    #[test]
    fn router_dispatches_by_family() {
        let router = Router::default()
            .with_evm(Box::new(RecordingSubmitter {
                domain_sep: [0; 32],
                delivered: Mutex::default(),
                submissions: Mutex::default(),
            }))
            .with_sui(Box::new(RecordingSubmitter {
                domain_sep: [0; 32],
                delivered: Mutex::default(),
                submissions: Mutex::default(),
            }));
        // EVM + Sui destinations both resolve; an unconfigured family errors.
        assert!(router.for_dst(HYPER_ID).is_ok());
        assert!(router.for_dst(SUI_ID).is_ok());
        assert!(Router::default().for_dst(HYPER_ID).is_err()); // nothing configured
    }

    #[tokio::test]
    async fn relays_then_dedups() {
        let ds = derive_domain_sep(&TEST_SALT);
        let signer = LocalSigner {
            signer: ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap(),
            domain_sep: ds,
        };
        let submitter =
            RecordingSubmitter { domain_sep: ds, delivered: Mutex::default(), submissions: Mutex::default() };
        let m = vector_message();

        // First pass delivers and submits the on-chain-valid signature.
        assert_eq!(
            relay_message(&m, &ds, &signer, &submitter).await.unwrap(),
            RelayOutcome::Delivered
        );
        let subs = submitter.submissions.lock().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].1.scheme_tag, bridge_types::envelope::SCHEME_ED25519);
        assert_eq!(
            hex::encode(&subs[0].1.signature),
            "12bc85a949906a86bdea305aa6bc32ef704e77de62ea5fb65a3df3a39902e533\
98ca95da28a3c34aa8187edcf8f6936330c94016e1e1c4d3f2f7b80027190001"
        );
        drop(subs);

        // Second pass sees it already delivered.
        assert_eq!(
            relay_message(&m, &ds, &signer, &submitter).await.unwrap(),
            RelayOutcome::AlreadyDelivered
        );
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
    }
}
