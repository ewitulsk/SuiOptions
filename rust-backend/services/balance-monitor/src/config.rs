use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use runtime_config::config_load;
use serde::Deserialize;
use sui_tx::Network;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Sui network the watched wallets live on.
    pub network: Network,

    /// `/health` + `/metrics` bind address.
    pub ops_addr: SocketAddr,

    /// Seconds between balance polls.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    #[serde(rename = "watch")]
    pub watches: Vec<Watch>,

    /// Protocol-holdings drain watches (SO-387). Optional; see
    /// `protocol_watch.rs` for what the fields mean and the top-level-field
    /// limitation.
    #[serde(default, rename = "drain_watch")]
    pub drain_watches: Vec<DrainWatch>,
}

/// One watched wallet. Exactly one of `secrets_file` / `address` must be
/// set: `secrets_file` derives the address from the same rendered secrets
/// TOML the owning service mounts (tracks key rotation automatically);
/// `address` is for wallets whose key never lands on this host.
#[derive(Debug, Clone, Deserialize)]
pub struct Watch {
    /// Label used in metrics and the `low-balance-<name>` alert_id.
    pub name: String,

    pub secrets_file: Option<PathBuf>,
    pub address: Option<String>,

    /// Below this many SUI the wallet is flagged low.
    pub low_balance_sui: f64,
}

/// One watched protocol object: alert when the summed balance fields drop
/// more than `drop_bps` below the window's max.
#[derive(Debug, Clone, Deserialize)]
pub struct DrainWatch {
    /// Label used in metrics and the `drain-suspected-<name>` alert_id.
    pub name: String,
    pub object_id: String,
    /// Top-level JSON fields summed into the watched total (e.g. a bucket's
    /// `underlying_balance`, `settlement_balance`).
    pub fields: Vec<String>,
    /// Basis-point drop from the in-window max that trips the alert.
    pub drop_bps: u64,
    #[serde(default = "default_drain_window")]
    pub window_secs: u64,
}

fn default_poll_interval() -> u64 {
    60
}

fn default_drain_window() -> u64 {
    3_600
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = config_load::load_toml(path)?;
        if cfg.watches.is_empty() {
            bail!("balance-monitor config has no [[watch] ] entries");
        }
        for w in &cfg.watches {
            if w.secrets_file.is_some() == w.address.is_some() {
                bail!(
                    "watch '{}' must set exactly one of secrets_file / address",
                    w.name
                );
            }
        }
        for d in &cfg.drain_watches {
            if d.fields.is_empty() {
                bail!("drain watch '{}' has no fields", d.name);
            }
            if d.drop_bps == 0 || d.drop_bps > 10_000 {
                bail!("drain watch '{}': drop_bps must be 1..=10000", d.name);
            }
        }
        Ok(cfg)
    }
}
