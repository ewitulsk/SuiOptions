//! EVM destination submitter: delivers a verified message to a HyperEVM
//! `Inbox.receiveMessage(message, envelope)` (the Sui → EVM direction). The
//! struct ABI must match `Inbox.sol` / `Message.sol` exactly.

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bridge_types::{Bytes32, CrossChainMessage, SignatureEnvelope};

use crate::relay::DestSubmitter;

sol! {
    #[sol(rpc)]
    contract IInbox {
        struct CrossChainMessage {
            uint8 version;
            uint32 srcChainId;
            uint32 dstChainId;
            uint64 nonce;
            bytes32 srcApp;
            bytes32 dstApp;
            bytes payload;
        }
        struct SignatureEnvelope {
            uint8 schemeTag;
            uint32 groupPubkeyId;
            bytes signature;
        }
        function receiveMessage(CrossChainMessage message, SignatureEnvelope envelope) external;
        function consumed(bytes32 messageHash) external view returns (bool);
    }
}

pub struct EvmDestSubmitter {
    provider: DynProvider,
    inbox: Address,
}

impl EvmDestSubmitter {
    pub fn new(rpc_url: &str, inbox: &str, relayer_key: &str) -> Result<Self> {
        let signer: PrivateKeySigner =
            relayer_key.trim_start_matches("0x").parse().context("parsing relayer key")?;
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(rpc_url.parse().context("parsing evm_rpc_url")?)
            .erased();
        Ok(Self { provider, inbox: inbox.parse().context("parsing evm_inbox_addr")? })
    }

    fn to_sol_message(m: &CrossChainMessage) -> IInbox::CrossChainMessage {
        IInbox::CrossChainMessage {
            version: m.version,
            srcChainId: m.src_chain_id,
            dstChainId: m.dst_chain_id,
            nonce: m.nonce,
            srcApp: FixedBytes::from(m.src_app),
            dstApp: FixedBytes::from(m.dst_app),
            payload: Bytes::from(m.payload.clone()),
        }
    }

    fn to_sol_envelope(e: &SignatureEnvelope) -> IInbox::SignatureEnvelope {
        IInbox::SignatureEnvelope {
            schemeTag: e.scheme_tag,
            groupPubkeyId: e.group_pubkey_id,
            signature: Bytes::from(e.signature.clone()),
        }
    }
}

#[async_trait]
impl DestSubmitter for EvmDestSubmitter {
    async fn is_delivered(&self, digest: &Bytes32) -> Result<bool> {
        let inbox = IInbox::new(self.inbox, &self.provider);
        let consumed = inbox.consumed(FixedBytes::from(*digest)).call().await?;
        Ok(consumed)
    }

    async fn submit(
        &self,
        message: &CrossChainMessage,
        envelope: &SignatureEnvelope,
    ) -> Result<()> {
        let inbox = IInbox::new(self.inbox, &self.provider);
        let pending = inbox
            .receiveMessage(Self::to_sol_message(message), Self::to_sol_envelope(envelope))
            .send()
            .await
            .context("sending receiveMessage")?;
        let receipt = pending.get_receipt().await.context("awaiting receipt")?;
        if !receipt.status() {
            bail!("receiveMessage reverted (tx {:?})", receipt.transaction_hash);
        }
        Ok(())
    }
}
