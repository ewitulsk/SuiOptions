//! Relay orchestration. For each committed source message: skip if the
//! destination already delivered it (gas saver — the on-chain `consumed` set is
//! the real guard), else fetch a signature and submit. The relayer is
//! untrusted; correctness never depends on it.

use anyhow::Result;
use async_trait::async_trait;
use bridge_types::{Bytes32, CrossChainMessage, SignatureEnvelope};
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

pub async fn relay_message(
    message: &CrossChainMessage,
    signer: &dyn RemoteSigner,
    submitter: &dyn DestSubmitter,
) -> Result<RelayOutcome> {
    let digest = message.digest();
    if submitter.is_delivered(&digest).await? {
        return Ok(RelayOutcome::AlreadyDelivered);
    }
    let envelope = signer.sign(message).await?;
    submitter.submit(message, &envelope).await?;
    Ok(RelayOutcome::Delivered)
}

/// One poll → relay pass. Returns the number of messages newly delivered. A
/// single message failing to relay is logged and skipped, not fatal.
pub async fn relay_once(
    watcher: &mut dyn SourceWatcher,
    signer: &dyn RemoteSigner,
    submitter: &dyn DestSubmitter,
) -> Result<usize> {
    let mut delivered = 0;
    for message in watcher.poll().await? {
        let digest = hex::encode(message.digest());
        match relay_message(&message, signer, submitter).await {
            Ok(RelayOutcome::Delivered) => {
                info!(digest = %digest, nonce = message.nonce, "relayed");
                delivered += 1;
            }
            Ok(RelayOutcome::AlreadyDelivered) => {
                info!(digest = %digest, "already delivered, skipping");
            }
            Err(e) => warn!(digest = %digest, error = %e, "relay failed, will retry next poll"),
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

    struct LocalSigner(ThresholdSigner);
    #[async_trait]
    impl RemoteSigner for LocalSigner {
        async fn sign(&self, m: &CrossChainMessage) -> Result<SignatureEnvelope> {
            Ok(self.0.sign(m, 1)?)
        }
    }

    #[derive(Default)]
    struct RecordingSubmitter {
        delivered: Mutex<HashSet<Bytes32>>,
        submissions: Mutex<Vec<(CrossChainMessage, SignatureEnvelope)>>,
    }
    #[async_trait]
    impl DestSubmitter for RecordingSubmitter {
        async fn is_delivered(&self, digest: &Bytes32) -> Result<bool> {
            Ok(self.delivered.lock().unwrap().contains(digest))
        }
        async fn submit(&self, m: &CrossChainMessage, e: &SignatureEnvelope) -> Result<()> {
            self.delivered.lock().unwrap().insert(m.digest());
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

    #[tokio::test]
    async fn relays_then_dedups() {
        let signer = LocalSigner(ThresholdSigner::from_seeds([0x42; 32], [0x11; 32]).unwrap());
        let submitter = RecordingSubmitter::default();
        let m = vector_message();

        // First pass delivers and submits the on-chain-valid signature.
        assert_eq!(relay_message(&m, &signer, &submitter).await.unwrap(), RelayOutcome::Delivered);
        let subs = submitter.submissions.lock().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].1.scheme_tag, bridge_types::envelope::SCHEME_ED25519);
        assert_eq!(
            hex::encode(&subs[0].1.signature),
            "3830227d4552e5a5864e7c16dbc69f0326a45a068cb98443fc4098824ce7afd0\
e0ccbe45043b269ae0b50c5bc54854451418d1803af9277c2262b9c0ad493b03"
        );
        drop(subs);

        // Second pass sees it already delivered.
        assert_eq!(
            relay_message(&m, &signer, &submitter).await.unwrap(),
            RelayOutcome::AlreadyDelivered
        );
        assert_eq!(submitter.submissions.lock().unwrap().len(), 1);
    }
}
