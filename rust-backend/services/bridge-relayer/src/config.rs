use std::path::Path;

use anyhow::{Context, Result};
use bridge_types::message::derive_domain_sep;
use bridge_types::Bytes32;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Sui JSON-RPC URL of the source chain to watch.
    pub source_rpc_url: String,
    /// Deployed bridge package id (holds the `events` module + Outbox).
    pub bridge_package_id: String,
    /// Base URL of the signer node (`bridge-signer-service`).
    pub signer_url: String,
    /// 32-byte hex per-deployment salt; the digest domain separator is
    /// `keccak256("XCHAIN_MSG_V1" || salt)` (spec §2.2). MUST match the value
    /// baked into the deployed contracts and the signer node.
    pub deployment_salt_hex: String,
    /// Seconds between source polls.
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,

    /// EVM destination (HyperEVM). When all three are set the relayer submits to
    /// the EVM Inbox; otherwise it dry-runs the destination.
    pub evm_rpc_url: Option<String>,
    pub evm_inbox_addr: Option<String>,
    pub evm_relayer_key: Option<String>,

    /// EVM *source* watcher (HyperEVM → Sui). When set, the relayer also polls
    /// this Outbox for committed messages.
    pub evm_source_rpc_url: Option<String>,
    pub evm_source_outbox_addr: Option<String>,
    #[serde(default)]
    pub evm_source_confirmations: u64,
    #[serde(default)]
    pub evm_source_start_block: u64,

    /// Sui destination submitter (EVM → Sui). When set, Sui-destined messages are
    /// delivered by calling the app's `bridge_receive` convention.
    pub sui_dest_rpc_url: Option<String>,
    pub sui_relayer_key: Option<String>,
    pub sui_inbox_id: Option<String>,
    pub sui_group_key_registry_id: Option<String>,
    #[serde(default = "default_sui_gas_budget")]
    pub sui_gas_budget: u64,
}

fn default_sui_gas_budget() -> u64 {
    100_000_000
}

fn default_poll_secs() -> u64 {
    5
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        config_load::load_toml(path)
    }

    /// Derive the digest domain separator from the configured deployment salt.
    pub fn domain_sep(&self) -> Result<Bytes32> {
        let bytes = hex::decode(self.deployment_salt_hex.trim_start_matches("0x"))
            .context("decoding deployment_salt_hex")?;
        let salt: Bytes32 = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("deployment_salt must be 32 bytes, got {}", v.len()))?;
        Ok(derive_domain_sep(&salt))
    }
}
