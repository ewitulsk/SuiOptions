use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Gates the
    /// auto-seed — only `dev` and `staging` seed the catalog from
    /// `deployments.json` testTokens. `prod` starts empty (mainnet tokens are
    /// added via the internal API).
    pub environment: String,

    /// Which slot in `deployments.json` to read `package_info` from
    /// (`testnet` / `mainnet` / `devnet`, case-insensitive).
    pub network: String,

    /// Path to `deployments.json`. token-info is the ONLY service that reads
    /// this file; everyone else reads from token-info.
    #[serde(default = "default_deployments_path")]
    pub deployments_path: PathBuf,

    /// Postgres connection string for the catalog. Standard libpq URL form;
    /// `${DB_PASSWORD}` / `${DB_HOST}` are expanded from the env at load time.
    pub database_url: String,

    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    /// Public read API bind address (proxied by nginx, reachable from the
    /// internet). Serves `/health`, `/tokens`, `/package-info`.
    pub public_bind_addr: SocketAddr,

    /// Internal mutate API bind address. NEVER proxied by nginx / routed by
    /// the ALB — reachable only container-to-container and over the VPN.
    pub internal_bind_addr: SocketAddr,

    /// CORS allow-list for the public API. `["*"]` permits any origin.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,
}

fn default_deployments_path() -> PathBuf {
    PathBuf::from("deployments.json")
}

fn default_db_pool_size() -> u32 {
    8
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }

    /// Auto-seed the catalog from testTokens only on dev/staging. Mainnet/prod
    /// never seeds synthetic tokens.
    pub fn should_seed(&self) -> bool {
        matches!(self.environment.to_ascii_lowercase().as_str(), "dev" | "staging")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_gate() {
        let mk = |env: &str| Config {
            environment: env.into(),
            network: "testnet".into(),
            deployments_path: "deployments.json".into(),
            database_url: "x".into(),
            db_pool_size: 8,
            public_bind_addr: "0.0.0.0:9005".parse().unwrap(),
            internal_bind_addr: "0.0.0.0:9006".parse().unwrap(),
            allowed_origins: vec!["*".into()],
        };
        assert!(mk("dev").should_seed());
        assert!(mk("staging").should_seed());
        assert!(mk("STAGING").should_seed());
        assert!(!mk("prod").should_seed());
    }
}
