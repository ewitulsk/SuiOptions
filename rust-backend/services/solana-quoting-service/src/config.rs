//! Solana quoting service configuration.
//!
//! Loaded from a TOML file passed via `--config`. Matches the Sui twin's
//! pattern so the two services share the same deploy/run ergonomics.
//!
//! The `Config` struct fields stay public so tests can construct one
//! in-process without touching the filesystem.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use runtime_config::config_load;
use serde::Deserialize;
use solana_token_info_client::TokenInfoClient;

#[derive(Clone, Debug)]
pub struct Config {
    /// Where the service accepts WS connections from retail + MM clients.
    pub bind_addr: SocketAddr,
    /// The solana-indexer's GraphQL endpoint. Account balances, signing keys,
    /// and bucket state are read just-in-time from here per request.
    pub indexer_graphql_url: String,
    /// How long an `RFQRequest` collects quotes before responding.
    pub rfq_window: Duration,
    /// How long a cached bulk-view premium stays fresh. A hit older than this
    /// is still returned (stale-while-revalidate) but triggers a background
    /// refresh to MMs. Spec default 30s.
    pub bulk_view_cache_ttl: Duration,
    /// How often the service sends `Ping`s to keep the WS connection live.
    pub ping_interval: Duration,
    /// solana-token-info public base URL. The protocol-id domain separator
    /// (the options_core Config PDA) is fetched from here at boot.
    pub token_info_url: String,
    /// Domain separator the on-chain quote carries: the options_core Config
    /// PDA, base58. Fetched at boot from solana-token-info
    /// (`Snapshot::config_pda`), exactly as the program derives it and the
    /// solana-mm-bot signs it, so it stays in lockstep with the deployment
    /// instead of a hand-copied string. Hard cutover: unreachable
    /// solana-token-info crashes the boot, no local fallback.
    pub protocol_id: String,
    /// Max concurrent in-flight RFQ orchestrations per retail connection.
    pub max_inflight_rfqs_per_session: usize,
    /// Max concurrent in-flight RFQ orchestrations across all retail
    /// connections. Backstop for the per-session limit.
    pub max_inflight_rfqs_global: usize,
}

/// On-disk TOML shape. Kept separate from [`Config`] so the public type can
/// stay ergonomic (`Duration`) while the file stays human-friendly (millis
/// as integers).
#[derive(Debug, Deserialize)]
struct FileConfig {
    bind_addr: SocketAddr,
    indexer_graphql_url: String,
    rfq_window_ms: u64,
    #[serde(default = "default_bulk_view_cache_ttl_ms")]
    bulk_view_cache_ttl_ms: u64,
    #[serde(default = "default_ping_interval_secs")]
    ping_interval_secs: u64,
    /// solana-token-info public base URL. The protocol-id domain separator is
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

fn default_bulk_view_cache_ttl_ms() -> u64 {
    30_000
}

fn default_max_inflight_per_session() -> usize {
    16
}

fn default_max_inflight_global() -> usize {
    256
}

impl Config {
    /// Load the TOML file, then fetch the `protocol_id` domain separator (the
    /// options_core Config PDA) from solana-token-info. Hard cutover off
    /// `solana-deployments.json`: if solana-token-info is unreachable after
    /// the retry window we propagate the error (the process crashes), there
    /// is no local fallback.
    pub async fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file: FileConfig = config_load::load_toml(path)?;

        let snapshot = TokenInfoClient::new(&file.token_info_url)
            .fetch_blocking_until_ready(30, Duration::from_secs(2))
            .await
            .with_context(|| {
                format!(
                    "fetching protocol_id from solana-token-info at {}",
                    file.token_info_url
                )
            })?;
        let protocol_id = snapshot.config_pda().to_string();

        Ok(Self {
            bind_addr: file.bind_addr,
            indexer_graphql_url: file.indexer_graphql_url,
            rfq_window: Duration::from_millis(file.rfq_window_ms),
            bulk_view_cache_ttl: Duration::from_millis(file.bulk_view_cache_ttl_ms),
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

    // `Config::load` fetches `protocol_id` from solana-token-info over the
    // network, so the round-trip test covers the on-disk TOML shape only
    // (the network fetch is exercised by solana-token-info-client's tests).
    #[test]
    fn parses_toml_file_shape() {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let path = dir.join(format!("sol-qs-config-{pid}.toml"));
        std::fs::write(
            &path,
            r#"
bind_addr           = "127.0.0.1:9999"
indexer_graphql_url = "http://example.com/graphql"
rfq_window_ms       = 1500
ping_interval_secs  = 20
token_info_url      = "http://127.0.0.1:9005"
"#,
        )
        .unwrap();
        let file: FileConfig = config_load::load_toml(&path).unwrap();
        assert_eq!(file.bind_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(file.indexer_graphql_url, "http://example.com/graphql");
        assert_eq!(file.rfq_window_ms, 1500);
        assert_eq!(file.ping_interval_secs, 20);
        assert_eq!(file.token_info_url, "http://127.0.0.1:9005");
        // Defaults apply when keys are missing.
        assert_eq!(file.max_inflight_rfqs_per_session, 16);
        assert_eq!(file.max_inflight_rfqs_global, 256);
        assert_eq!(file.bulk_view_cache_ttl_ms, 30_000);
        std::fs::remove_file(&path).ok();
    }
}
