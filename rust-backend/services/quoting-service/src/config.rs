//! Quoting service configuration.
//!
//! Loaded from a TOML file at `CONFIG_PATH` (default `config/testnet.toml`).
//! Matches the indexer's pattern so the two services share the same
//! deploy/run ergonomics.
//!
//! The `Config` struct fields stay public so tests can construct one
//! in-process without touching the filesystem.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use runtime_config::config_load;
use serde::Deserialize;
use token_info_client::TokenInfoClient;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the service accepts WS connections from retail + MM clients.
    pub bind_addr: SocketAddr,
    /// The indexer's WS endpoint. Subscribed to from boot.
    pub indexer_url: String,
    /// How long an `RFQRequest` collects quotes before responding.
    pub rfq_window: Duration,
    /// How often the service sends `Ping`s to keep the WS connection live.
    pub ping_interval: Duration,
    /// token-info public base URL. The protocol-id domain separator (the
    /// AdminCap object id bytes) is fetched from here at boot, replacing the
    /// old `deployments.json` read.
    pub token_info_url: String,
    /// Domain-separator bytes the on-chain `ProtocolConfig.protocol_id`
    /// holds — used to short-circuit-reject quotes whose `protocol_id` is
    /// wrong before the chain has to. Fetched at boot from token-info (the
    /// AdminCap object id, exactly as the contract derives it in
    /// `admin.move::init` and the mm-bot signs it), so it stays in lockstep
    /// with the deployment instead of a hand-copied string.
    pub protocol_id: Vec<u8>,
    /// Max concurrent in-flight RFQ orchestrations per retail connection.
    /// A misbehaving client can otherwise spawn one tokio task per RFQ
    /// without bound (see SO-65).
    pub max_inflight_rfqs_per_session: usize,
    /// Max concurrent in-flight RFQ orchestrations across all retail
    /// connections. Backstop for the per-session limit.
    pub max_inflight_rfqs_global: usize,
}

/// On-disk TOML shape. Kept separate from [`Config`] so the public type can
/// stay ergonomic (`Duration`, `Vec<u8>`) while the file stays human-friendly
/// (millis as integers, network slot as a name).
#[derive(Debug, Deserialize)]
struct FileConfig {
    bind_addr: SocketAddr,
    indexer_url: String,
    rfq_window_ms: u64,
    #[serde(default = "default_ping_interval_secs")]
    ping_interval_secs: u64,
    /// token-info public base URL. The protocol-id domain separator is
    /// fetched from here at boot.
    token_info_url: String,
    #[serde(default = "default_max_inflight_per_session")]
    max_inflight_rfqs_per_session: usize,
    #[serde(default = "default_max_inflight_global")]
    max_inflight_rfqs_global: usize,
}

fn default_ping_interval_secs() -> u64 {
    15
}

fn default_max_inflight_per_session() -> usize {
    16
}

fn default_max_inflight_global() -> usize {
    256
}

impl Config {
    /// Load the TOML file, then fetch the `protocol_id` domain separator from
    /// token-info. Hard cutover off `deployments.json`: if token-info is
    /// unreachable after the retry window we propagate the error (the process
    /// crashes), there is no local fallback.
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file: FileConfig = config_load::load_toml(path)?;

        let snapshot = TokenInfoClient::new(&file.token_info_url)
            .fetch_blocking_until_ready(30, Duration::from_secs(2))
            .await
            .with_context(|| {
                format!("fetching protocol_id from token-info at {}", file.token_info_url)
            })?;
        let protocol_id = snapshot.protocol_id_bytes()?;

        Ok(Self {
            bind_addr: file.bind_addr,
            indexer_url: file.indexer_url,
            rfq_window: Duration::from_millis(file.rfq_window_ms),
            ping_interval: Duration::from_secs(file.ping_interval_secs),
            token_info_url: file.token_info_url,
            protocol_id,
            max_inflight_rfqs_per_session: file.max_inflight_rfqs_per_session,
            max_inflight_rfqs_global: file.max_inflight_rfqs_global,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Config::load` now fetches `protocol_id` from token-info over the
    // network, so the round-trip test covers the on-disk TOML shape only
    // (the network fetch is exercised by token-info-client's own tests).
    #[test]
    fn parses_toml_file_shape() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let path = dir.join(format!("qs-config-{pid}.toml"));
        std::fs::write(
            &path,
            r#"
bind_addr      = "127.0.0.1:9999"
indexer_url    = "ws://example.com/feed"
rfq_window_ms  = 1500
ping_interval_secs = 20
token_info_url = "http://127.0.0.1:9005"
"#,
        )
        .unwrap();
        let file: FileConfig = config_load::load_toml(&path).unwrap();
        assert_eq!(file.bind_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(file.indexer_url, "ws://example.com/feed");
        assert_eq!(file.rfq_window_ms, 1500);
        assert_eq!(file.ping_interval_secs, 20);
        assert_eq!(file.token_info_url, "http://127.0.0.1:9005");
        // Defaults apply when keys are missing.
        assert_eq!(file.max_inflight_rfqs_per_session, 16);
        assert_eq!(file.max_inflight_rfqs_global, 256);
        std::fs::remove_file(&path).ok();
    }
}
