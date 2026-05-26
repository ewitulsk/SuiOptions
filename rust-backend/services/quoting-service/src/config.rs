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
use serde::Deserialize;

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
    /// Domain-separator bytes the on-chain `ProtocolConfig.protocol_id`
    /// holds — used to short-circuit-reject quotes whose `protocol_id` is
    /// wrong before the chain has to.
    pub protocol_id: Vec<u8>,
    /// Max concurrent in-flight RFQ orchestrations per retail connection.
    /// A misbehaving client can otherwise spawn one tokio task per RFQ
    /// without bound (see SO-65).
    pub max_inflight_rfqs_per_session: usize,
    /// Max concurrent in-flight RFQ orchestrations across all retail
    /// connections. Backstop for the per-session limit.
    pub max_inflight_rfqs_global: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:9002".parse().unwrap(),
            indexer_url: "ws://127.0.0.1:9001/".to_string(),
            rfq_window: Duration::from_secs(2),
            ping_interval: Duration::from_secs(15),
            protocol_id: b"sui-options-protocol-v0".to_vec(),
            max_inflight_rfqs_per_session: 16,
            max_inflight_rfqs_global: 256,
        }
    }
}

/// On-disk TOML shape. Kept separate from [`Config`] so the public type can
/// stay ergonomic (`Duration`, `Vec<u8>`) while the file stays human-friendly
/// (millis as integers, protocol_id as a string).
#[derive(Debug, Deserialize)]
struct FileConfig {
    bind_addr: SocketAddr,
    indexer_url: String,
    rfq_window_ms: u64,
    #[serde(default = "default_ping_interval_secs")]
    ping_interval_secs: u64,
    /// Either a UTF-8 string (the common case — domain separators are
    /// human-readable) or a `0x`-prefixed hex blob.
    protocol_id: String,
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
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let settings = config::Config::builder()
            .add_source(config::File::from(path).required(true))
            .build()
            .with_context(|| format!("loading config {}", path.display()))?;
        let file: FileConfig = settings
            .try_deserialize()
            .with_context(|| format!("parsing config {}", path.display()))?;

        Ok(Self {
            bind_addr: file.bind_addr,
            indexer_url: file.indexer_url,
            rfq_window: Duration::from_millis(file.rfq_window_ms),
            ping_interval: Duration::from_secs(file.ping_interval_secs),
            protocol_id: parse_protocol_id(&file.protocol_id)?,
            max_inflight_rfqs_per_session: file.max_inflight_rfqs_per_session,
            max_inflight_rfqs_global: file.max_inflight_rfqs_global,
        })
    }
}

fn parse_protocol_id(s: &str) -> Result<Vec<u8>> {
    if let Some(hex_body) = s.strip_prefix("0x") {
        hex::decode(hex_body).with_context(|| format!("protocol_id is not valid hex: {s:?}"))
    } else {
        Ok(s.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_protocol_id() {
        assert_eq!(parse_protocol_id("hello").unwrap(), b"hello".to_vec());
    }

    #[test]
    fn parses_hex_protocol_id() {
        assert_eq!(parse_protocol_id("0xdeadbeef").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn loads_toml_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("qs-config-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
bind_addr     = "127.0.0.1:9999"
indexer_url   = "ws://example.com/feed"
rfq_window_ms = 1500
ping_interval_secs = 20
protocol_id   = "test-domain"
"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(cfg.indexer_url, "ws://example.com/feed");
        assert_eq!(cfg.rfq_window, Duration::from_millis(1500));
        assert_eq!(cfg.ping_interval, Duration::from_secs(20));
        assert_eq!(cfg.protocol_id, b"test-domain".to_vec());
        // Defaults apply when keys are missing.
        assert_eq!(cfg.max_inflight_rfqs_per_session, 16);
        assert_eq!(cfg.max_inflight_rfqs_global, 256);
        std::fs::remove_file(&path).ok();
    }
}
