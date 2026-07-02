use std::path::Path;

use anyhow::Result;
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
    /// Seconds between source polls.
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,

    /// EVM destination (HyperEVM). When all three are set the relayer submits to
    /// the EVM Inbox; otherwise it dry-runs the destination.
    pub evm_rpc_url: Option<String>,
    pub evm_inbox_addr: Option<String>,
    pub evm_relayer_key: Option<String>,
}

fn default_poll_secs() -> u64 {
    5
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        config_load::load_toml(path)
    }
}
