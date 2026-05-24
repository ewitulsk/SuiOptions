use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    /// HTTP bind address (frontend hits this).
    pub bind_addr: SocketAddr,
    /// Indexer WS endpoint. Subscribed from boot.
    pub indexer_url: String,
    /// CORS allow-list. `["*"]` permits any origin (dev only).
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,
    /// Sui network slot (`testnet` / `mainnet` / `devnet`) to read from
    /// `deployments.json`. Selects which token catalog we use to resolve
    /// coin types to {symbol, decimals}.
    pub network: String,
    /// Path to `deployments.json` (committed at the workspace root). The
    /// api-service only reads it — never writes.
    #[serde(default = "default_deployments_path")]
    pub deployments_path: PathBuf,
}

fn default_deployments_path() -> PathBuf {
    PathBuf::from("deployments.json")
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let settings = config::Config::builder()
            .add_source(config::File::from(path).required(true))
            .build()
            .with_context(|| format!("loading config {}", path.display()))?;
        settings
            .try_deserialize()
            .with_context(|| format!("parsing config {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("api-service-config-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
bind_addr        = "127.0.0.1:9003"
indexer_url      = "ws://127.0.0.1:9001/"
allowed_origins  = ["http://localhost:5173"]
network          = "testnet"
deployments_path = "deployments.json"
"#,
        )
        .unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9003");
        assert_eq!(cfg.indexer_url, "ws://127.0.0.1:9001/");
        assert_eq!(cfg.allowed_origins, vec!["http://localhost:5173".to_string()]);
        assert_eq!(cfg.network, "testnet");
        std::fs::remove_file(&path).ok();
    }
}
