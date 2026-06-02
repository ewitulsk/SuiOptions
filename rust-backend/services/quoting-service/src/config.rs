//! Quoting service configuration.
//!
//! Loaded from a TOML file at `CONFIG_PATH` (default `config/testnet.toml`).
//! Matches the indexer's pattern so the two services share the same
//! deploy/run ergonomics.
//!
//! The `Config` struct fields stay public so tests can construct one
//! in-process without touching the filesystem.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    /// wrong before the chain has to. Resolved at load time from
    /// `deployments.json` (the AdminCap object id, exactly as the contract
    /// derives it in `admin.move::init` and the mm-bot signs it), so it
    /// stays in lockstep with the deployment instead of a hand-copied
    /// string.
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
    /// Which slot in `deployments.json` to resolve `protocol_id` from.
    /// Accepted values: `mainnet`, `testnet`, `devnet` (case-insensitive).
    network: String,
    /// Path to `deployments.json`. Relative paths resolve from the
    /// process's working directory — the container's `WORKDIR /app`
    /// matches the image's `/app/deployments.json`.
    #[serde(default = "default_deployments_path")]
    deployments_path: PathBuf,
    #[serde(default = "default_max_inflight_per_session")]
    max_inflight_rfqs_per_session: usize,
    #[serde(default = "default_max_inflight_global")]
    max_inflight_rfqs_global: usize,
}

fn default_ping_interval_secs() -> u64 {
    15
}

fn default_deployments_path() -> PathBuf {
    PathBuf::from("deployments.json")
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
            protocol_id: resolve_protocol_id(&file.deployments_path, &file.network)?,
            max_inflight_rfqs_per_session: file.max_inflight_rfqs_per_session,
            max_inflight_rfqs_global: file.max_inflight_rfqs_global,
        })
    }
}

/// Read `deployments.json` and derive `protocol_id` for `network` exactly
/// as the chain does (`admin.move::init`: the AdminCap object id as bytes)
/// and the mm-bot does (`NetworkDeployment::protocol_id_bytes`). Keeping
/// all three on the same source is what prevents `ProtocolMismatch`
/// rejections after a redeploy.
fn resolve_protocol_id(deployments_path: &Path, network: &str) -> Result<Vec<u8>> {
    let dep = deployments::Deployments::load(deployments_path)?;
    let net = dep.for_network(network).with_context(|| {
        format!(
            "resolving network {network} in {}",
            deployments_path.display()
        )
    })?;
    net.protocol_id_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_toml_round_trip() {
        // protocol_id is the AdminCap object id, byte-for-byte.
        let admin_cap = "0xda7ebab81868f7654daabf99c7e99dd44d5a9fb0da5ff179e5a0341276559a16";
        let expected = hex::decode(admin_cap.trim_start_matches("0x")).unwrap();

        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let deployments_path = dir.join(format!("qs-deployments-{pid}.json"));
        std::fs::write(
            &deployments_path,
            format!(
                r#"{{
  "mainnet": null,
  "testnet": {{
    "package_info": {{
      "packageId": "0x183ea56e98cb03af50bf3db7328035ac7ec94bb8dc11026d43a2c1ad641056b6",
      "adminCapId": "{admin_cap}",
      "protocolConfigId": "0xeb3ea89b93a627ddccce5e746a5e11d2833c20640fadadc732e2b0ae04f10b6f",
      "upgradeCapId": "0x681818aa5c1176a5c9c0d49e6f90211c63537d30ab7356b49f545b66f7fbfea0",
      "publishDigest": "ycZcKEXGMyX13zjba3zM2g7T8bH1dsWWSznkpKLJ2ND",
      "deployer": "0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865",
      "deployedAt": "2026-06-02T02:51:48.574422195+00:00",
      "network": "testnet"
    }}
  }},
  "devnet": null
}}"#
            ),
        )
        .unwrap();

        let path = dir.join(format!("qs-config-{pid}.toml"));
        std::fs::write(
            &path,
            format!(
                r#"
bind_addr     = "127.0.0.1:9999"
indexer_url   = "ws://example.com/feed"
rfq_window_ms = 1500
ping_interval_secs = 20
network       = "testnet"
deployments_path = "{}"
"#,
                deployments_path.display()
            ),
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9999");
        assert_eq!(cfg.indexer_url, "ws://example.com/feed");
        assert_eq!(cfg.rfq_window, Duration::from_millis(1500));
        assert_eq!(cfg.ping_interval, Duration::from_secs(20));
        assert_eq!(cfg.protocol_id, expected);
        // Defaults apply when keys are missing.
        assert_eq!(cfg.max_inflight_rfqs_per_session, 16);
        assert_eq!(cfg.max_inflight_rfqs_global, 256);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&deployments_path).ok();
    }
}
