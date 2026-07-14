use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use runtime_config::config_load;
use serde::Deserialize;
use solana_tx::Network;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Solana cluster the watched wallets live on.
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

    /// Below this many SOL the wallet is flagged low.
    pub low_balance_sol: f64,
}

fn default_poll_interval() -> u64 {
    60
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = config_load::load_toml(path)?;
        if cfg.watches.is_empty() {
            bail!("solana-balance-monitor config has no [[watch]] entries");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "solana-balance-monitor-{name}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    const HEADER: &str = "environment = \"dev\"\nnetwork = \"devnet\"\nops_addr = \"127.0.0.1:9012\"\n";

    #[test]
    fn loads_valid_config_with_default_interval() {
        let path = write_tmp(
            "valid",
            &format!(
                "{HEADER}\n[[watch]]\nname = \"solana-gas-station\"\nsecrets_file = \"/run/secrets/solana-gas-station.toml\"\nlow_balance_sol = 5.0\n\n[[watch]]\nname = \"solana-keeper\"\naddress = \"11111111111111111111111111111111\"\nlow_balance_sol = 2.0\n"
            ),
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.network, Network::Devnet);
        assert_eq!(cfg.poll_interval_secs, 60); // default
        assert_eq!(cfg.watches.len(), 2);
        assert_eq!(cfg.watches[0].low_balance_sol, 5.0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_watch_with_both_sources() {
        let path = write_tmp(
            "both",
            &format!(
                "{HEADER}\n[[watch]]\nname = \"x\"\nsecrets_file = \"a.toml\"\naddress = \"11111111111111111111111111111111\"\nlow_balance_sol = 1.0\n"
            ),
        );
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("exactly one of"), "{err}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_watch_with_neither_source() {
        let path = write_tmp(
            "neither",
            &format!("{HEADER}\n[[watch]]\nname = \"x\"\nlow_balance_sol = 1.0\n"),
        );
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("exactly one of"), "{err}");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_empty_watch_list() {
        // Explicit empty array exercises the validation bail; an absent key
        // already fails deserialization ("missing field `watch`").
        let path = write_tmp("empty", &format!("{HEADER}watch = []\n"));
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("no [[watch]]"), "{err}");
        std::fs::remove_file(path).ok();

        let missing = write_tmp("missing-watch", HEADER);
        assert!(Config::load(&missing).is_err());
        std::fs::remove_file(missing).ok();
    }
}
