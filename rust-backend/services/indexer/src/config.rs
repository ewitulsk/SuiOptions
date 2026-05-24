//! Indexer configuration.
//!
//! Loaded from a TOML file (default `config/testnet.toml`). The package id
//! is **not** in this file — it's resolved at runtime from
//! `deployments.json` based on `network`, so re-deploying the contracts
//! doesn't require editing the indexer config.
//!
//! Pattern is borrowed from Pismo's indexer, plus the fanout-specific
//! fields unique to this protocol.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use runtime_config::config_load;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Which slot in `deployments.json` to resolve the `package_id` from.
    /// Accepted values: `mainnet`, `testnet`, `devnet` (case-insensitive).
    pub network: String,

    /// Path to `deployments.json`. Relative paths resolve from the
    /// process's working directory — running the binary from
    /// `rust-backend/` matches the default.
    #[serde(default = "default_deployments_path")]
    pub deployments_path: PathBuf,

    /// Sui checkpoint remote store. Production-facing values:
    ///   `https://checkpoints.testnet.sui.io`
    ///   `https://checkpoints.mainnet.sui.io`
    pub remote_store_url: String,

    /// JSON-RPC endpoint for the same network. Used only to discover the
    /// latest checkpoint when `start_checkpoint` is unset. If omitted it
    /// is derived from `network` (mainnet/testnet/devnet default URLs).
    #[serde(default)]
    pub rpc_url: Option<String>,

    /// Checkpoint sequence number to start ingesting from. `0` is genesis;
    /// any non-zero value resumes from that checkpoint. **Unset** means
    /// "start from the current tip" — the indexer queries
    /// `sui_getLatestCheckpointSequenceNumber` at boot and uses that.
    #[serde(default)]
    pub start_checkpoint: Option<u64>,

    /// How many checkpoints the framework processes in parallel.
    pub concurrency: usize,

    /// Bind address for the WS fanout (quoting service subscribes here).
    pub fanout_addr: SocketAddr,

    /// How often the fanout publishes a `Heartbeat` frame.
    #[serde(default = "default_heartbeat_secs", rename = "heartbeat_interval_secs")]
    heartbeat_interval_secs: u64,

    /// Postgres connection string for the persistence layer. Standard libpq
    /// URL form, e.g. `postgresql://postgres:postgres@localhost:7654/indexer`.
    pub database_url: String,

    /// HTTP health-check bind address. The uptime dashboard and ALB target
    /// groups hit `GET /health` here. Defaults to `0.0.0.0:8081`.
    #[serde(default = "default_health_addr")]
    pub health_addr: SocketAddr,

    /// r2d2 pool size for Postgres connections. The worker holds at most one
    /// connection at a time; the fanout's cold path (history older than the
    /// in-memory tail) can hold additional ones briefly.
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    /// How many of the most recent persisted events to keep in the in-memory
    /// log on boot. The fanout serves snapshots from this tail; anything
    /// older is served from Postgres via [`crate::db::Repo::events_after`].
    #[serde(default = "default_recent_log_capacity")]
    pub recent_log_capacity: usize,
}

fn default_deployments_path() -> PathBuf {
    PathBuf::from("deployments.json")
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }

    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_secs)
    }

    /// JSON-RPC URL to query for the latest checkpoint. Returns the
    /// explicit `rpc_url` if set, else the public Sui endpoint for
    /// `network`.
    pub fn resolve_rpc_url(&self) -> Result<String> {
        if let Some(u) = &self.rpc_url {
            return Ok(u.clone());
        }
        Ok(match self.network.to_ascii_lowercase().as_str() {
            "mainnet" => sui_sdk::SUI_MAINNET_URL.to_string(),
            "testnet" => sui_sdk::SUI_TESTNET_URL.to_string(),
            "devnet" => sui_sdk::SUI_DEVNET_URL.to_string(),
            other => {
                return Err(anyhow::anyhow!(
                    "no default RPC for network {other}; set `rpc_url` explicitly"
                ));
            }
        })
    }

    /// Read `deployments.json` and pull the `package_id` for the configured
    /// `network`. Surfaced as `String` because that's the form the
    /// event-type builder needs (`{package_id}::events::EventName`).
    pub fn resolve_package_id(&self) -> Result<String> {
        let dep = deployments::Deployments::load(&self.deployments_path)?;
        let net = dep.for_network(&self.network).with_context(|| {
            format!(
                "resolving network {} in {}",
                self.network,
                self.deployments_path.display()
            )
        })?;
        Ok(net.package_info.package_id.clone())
    }
}

fn default_health_addr() -> SocketAddr {
    "0.0.0.0:8081".parse().unwrap()
}

fn default_heartbeat_secs() -> u64 {
    5
}

fn default_db_pool_size() -> u32 {
    8
}

fn default_recent_log_capacity() -> usize {
    1024
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("indexer-cfg-{}-{}.toml", std::process::id(), name));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn start_checkpoint_is_optional() {
        let p = write_tmp(
            "no_start",
            r#"
network = "testnet"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
fanout_addr = "127.0.0.1:9001"
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.start_checkpoint.is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn start_checkpoint_parses_when_set() {
        let p = write_tmp(
            "with_start",
            r#"
network = "testnet"
remote_store_url = "https://checkpoints.testnet.sui.io"
start_checkpoint = 12345
concurrency = 5
fanout_addr = "127.0.0.1:9001"
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.start_checkpoint, Some(12345));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rpc_url_defaults_per_network() {
        let p = write_tmp(
            "rpc_default",
            r#"
network = "testnet"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
fanout_addr = "127.0.0.1:9001"
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.resolve_rpc_url().unwrap().contains("testnet"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rpc_url_explicit_overrides_default() {
        let p = write_tmp(
            "rpc_explicit",
            r#"
network = "testnet"
remote_store_url = "https://checkpoints.testnet.sui.io"
rpc_url = "https://my-private-fullnode.example.com:443"
concurrency = 5
fanout_addr = "127.0.0.1:9001"
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(
            cfg.resolve_rpc_url().unwrap(),
            "https://my-private-fullnode.example.com:443"
        );
        std::fs::remove_file(&p).ok();
    }
}
