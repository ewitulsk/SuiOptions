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

fn default_poll_interval() -> u64 {
    60
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
        Ok(cfg)
    }
}
