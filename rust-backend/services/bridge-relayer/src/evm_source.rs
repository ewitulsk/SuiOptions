//! EVM source watcher (the HyperEVM → Sui direction). Polls the registered
//! Outbox for `MessageCommitted` logs past a confirmation depth, reconstructs
//! each `CrossChainMessage`, and hash-checks it against the indexed
//! `messageHash` topic before handing it to the relay loop.
//!
//! Uses alloy (already a relayer dep) so the non-indexed event fields ABI-decode
//! cleanly, rather than hand-rolling ABI parsing over raw JSON-RPC.

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::Filter;
use alloy::sol;
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result};
use async_trait::async_trait;
use bridge_types::{Bytes32, CrossChainMessage};
use tracing::warn;

use crate::relay::SourceWatcher;

sol! {
    #[sol(rpc)]
    event MessageCommitted(
        bytes32 indexed messageHash,
        uint32 srcChainId,
        uint32 dstChainId,
        uint64 nonce,
        bytes32 srcApp,
        bytes32 dstApp,
        bytes payload
    );
}

pub struct EvmSourceWatcher {
    provider: DynProvider,
    outbox: Address,
    domain_sep: Bytes32,
    /// Confirmation depth before a committed message is considered final (§4).
    confirmations: u64,
    /// Next block to scan from (advances past each polled, finalized range).
    from_block: u64,
}

impl EvmSourceWatcher {
    /// Connect a read-only provider and start scanning at `start_block`.
    pub async fn connect(
        rpc_url: &str,
        outbox: &str,
        domain_sep: Bytes32,
        confirmations: u64,
        start_block: u64,
    ) -> Result<Self> {
        let provider = ProviderBuilder::new()
            .connect_http(rpc_url.parse().context("parsing evm rpc url")?)
            .erased();
        let chain_id = provider.get_chain_id().await.context("evm get_chain_id")?;
        tracing::info!(rpc_url, chain_id, outbox, "connected to EVM source RPC");
        Ok(Self {
            provider,
            outbox: outbox.parse().context("parsing outbox address")?,
            domain_sep,
            confirmations,
            from_block: start_block,
        })
    }
}

#[async_trait]
impl SourceWatcher for EvmSourceWatcher {
    async fn poll(&mut self) -> Result<Vec<CrossChainMessage>> {
        let latest = self.provider.get_block_number().await.context("evm get_block_number")?;
        // Only scan blocks with at least `confirmations` on top of them.
        let safe_to = latest.saturating_sub(self.confirmations);
        if safe_to < self.from_block {
            return Ok(vec![]);
        }

        let filter = Filter::new()
            .address(self.outbox)
            .event_signature(MessageCommitted::SIGNATURE_HASH)
            .from_block(self.from_block)
            .to_block(safe_to);
        let logs = self.provider.get_logs(&filter).await.context("evm get_logs")?;

        let mut out = Vec::new();
        for log in logs {
            let ev = match log.log_decode::<MessageCommitted>() {
                Ok(d) => d.inner.data,
                Err(e) => {
                    warn!(error = %e, "skipping undecodable MessageCommitted log");
                    continue;
                }
            };
            let msg = CrossChainMessage::new(
                ev.srcChainId,
                ev.dstChainId,
                ev.nonce,
                ev.srcApp.0,
                ev.dstApp.0,
                ev.payload.to_vec(),
            );
            // Untrusted plumbing: the reconstructed message must hash to the
            // indexed messageHash the Outbox committed.
            if msg.digest(&self.domain_sep) != ev.messageHash.0 {
                warn!("skipping MessageCommitted log whose fields do not match its messageHash");
                continue;
            }
            out.push(msg);
        }

        self.from_block = safe_to + 1;
        Ok(out)
    }
}
