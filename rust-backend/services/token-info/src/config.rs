use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Gates the
    /// dev/staging test-token overlay on `/tokens` — only `dev` and `staging`
    /// fold the `deployments.json` testTokens into the catalog response.
    /// `prod` serves the DB catalog only.
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

    /// Display metadata (name, logo) for the dev/staging test-token overlay,
    /// keyed by ticker (e.g. `TBTC`). testTokens carry no name/logo, so this
    /// is where they come from. Ignored on prod (no overlay).
    #[serde(default)]
    pub seed_meta: BTreeMap<String, SeedMeta>,
}

/// Optional display metadata for an overlaid test token.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SeedMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub logo_uri: Option<String>,
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

    /// Overlay the deployments.json test tokens onto `/tokens` only on
    /// dev/staging. Mainnet/prod serves the durable DB catalog alone.
    pub fn overlay_test_tokens(&self) -> bool {
        matches!(
            self.environment.to_ascii_lowercase().as_str(),
            "dev" | "staging"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_gate() {
        let mk = |env: &str| Config {
            environment: env.into(),
            network: "testnet".into(),
            deployments_path: "deployments.json".into(),
            database_url: "x".into(),
            db_pool_size: 8,
            public_bind_addr: "0.0.0.0:9005".parse().unwrap(),
            internal_bind_addr: "0.0.0.0:9006".parse().unwrap(),
            allowed_origins: vec!["*".into()],
            seed_meta: BTreeMap::new(),
        };
        assert!(mk("dev").overlay_test_tokens());
        assert!(mk("staging").overlay_test_tokens());
        assert!(mk("STAGING").overlay_test_tokens());
        assert!(!mk("prod").overlay_test_tokens());
    }
}
