//! Indexer configuration.
//!
//! Loaded from a TOML file (default `config/testnet.toml`). Pattern is
//! borrowed from Pismo's indexer — same shape so anyone familiar with that
//! codebase can read it. The fanout-specific fields are unique to this
//! protocol: we still expose a WS for the quoting service after ingesting.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `0x…` hex package id of the deployed `options_protocol` Move package.
    /// Used to build the fully-qualified Move event type strings the worker
    /// matches against.
    pub package_id: String,

    /// Sui checkpoint remote store. Production-facing values:
    ///   `https://checkpoints.testnet.sui.io`
    ///   `https://checkpoints.mainnet.sui.io`
    pub remote_store_url: String,

    /// Checkpoint sequence number to start ingesting from. `0` is genesis;
    /// any non-zero value resumes from that checkpoint.
    pub start_checkpoint: u64,

    /// How many checkpoints the framework processes in parallel.
    pub concurrency: usize,

    /// Bind address for the WS fanout (quoting service subscribes here).
    pub fanout_addr: SocketAddr,

    /// How often the fanout publishes a `Heartbeat` frame.
    #[serde(default = "default_heartbeat_secs", rename = "heartbeat_interval_secs")]
    heartbeat_interval_secs: u64,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let builder = config::Config::builder()
            .add_source(config::File::from(path).required(true));
        let settings = builder
            .build()
            .with_context(|| format!("loading config {}", path.display()))?;
        settings
            .try_deserialize()
            .with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_secs)
    }
}

fn default_heartbeat_secs() -> u64 {
    5
}
