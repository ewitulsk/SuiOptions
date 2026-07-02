//! Destination submitters.
//!
//! M1 ships only [`DryRunSubmitter`]: it logs the message + signature it *would*
//! submit, so the relayer can run live against a deployed source Outbox and
//! demonstrate the watch → sign path. The real submitters are the next step:
//! - EVM Inbox: needs an EVM client (none vendored yet).
//! - Sui Inbox: needs the Layer-2 Locker to consume the delivered hot potato.

use anyhow::Result;
use async_trait::async_trait;
use bridge_types::{Bytes32, CrossChainMessage, SignatureEnvelope};
use tracing::info;

use crate::relay::DestSubmitter;

pub struct DryRunSubmitter;

#[async_trait]
impl DestSubmitter for DryRunSubmitter {
    async fn is_delivered(&self, _digest: &Bytes32) -> Result<bool> {
        Ok(false)
    }

    async fn submit(
        &self,
        message: &CrossChainMessage,
        envelope: &SignatureEnvelope,
    ) -> Result<()> {
        info!(
            dst_chain_id = message.dst_chain_id,
            nonce = message.nonce,
            scheme_tag = envelope.scheme_tag,
            group_pubkey_id = envelope.group_pubkey_id,
            message_hash = %format!("0x{}", hex::encode(message.digest())),
            signature = %format!("0x{}", hex::encode(&envelope.signature)),
            "DRY RUN — would submit to destination Inbox (real adapter pending)"
        );
        Ok(())
    }
}
