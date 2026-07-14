//! solana-indexer configuration.
//!
//! Loaded from a TOML file (default `config/config.toml`) via
//! `runtime_config::config_load` (`${VAR}` env expansion). Program ids
//! live here — unlike the Sui indexer there's no token-info hop yet; a
//! program redeploy on Solana keeps its id, so the ids are deploy-stable.
//!
//! The Helius API key is NOT here — it comes from the secrets file
//! rendered by render-secrets.sh (see [`Secrets`]).

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Cluster identifier (informational; the endpoint decides where the
    /// stream comes from): `mainnet` or `devnet`.
    pub cluster: String,

    /// LaserStream gRPC endpoint for the cluster, e.g.
    /// `https://laserstream-devnet-ewr.helius-rpc.com`. Region list:
    /// https://www.helius.dev/docs/laserstream — pick the closest.
    pub laserstream_endpoint: String,

    /// Deployed program ids (base58).
    pub programs: Programs,

    /// Slot to start ingesting from on a FRESH database. **Unset** means
    /// "tail from the stream tip". A resumed database ignores this and
    /// continues from its finalized watermark.
    #[serde(default)]
    pub start_slot: Option<u64>,

    /// Bind address for the GraphQL query API.
    #[serde(default = "default_graphql_addr")]
    pub graphql_addr: SocketAddr,

    /// CORS allow-list for the GraphQL API. `["*"]` (the default) allows
    /// any origin — matches the other public services.
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,

    /// Serve the GraphiQL playground and leave introspection on.
    /// Dev/staging opt in; keep off in prod.
    #[serde(default)]
    pub expose_playground: bool,

    /// Postgres connection string.
    pub database_url: String,

    /// HTTP ops bind address (`/health` + `/metrics`).
    #[serde(default = "default_health_addr")]
    pub health_addr: SocketAddr,

    /// r2d2 pool size. The worker holds one connection at a time; GraphQL
    /// resolvers hold additional ones briefly.
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Programs {
    pub options_core: String,
    pub auction_venue: String,
    pub options_vault: String,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}

/// Secrets file rendered by render-secrets.sh from the
/// `options/<env>/solana-indexer` secret. Only the Helius key lives here.
#[derive(Debug, Clone, Deserialize)]
pub struct Secrets {
    pub helius: HeliusSecrets,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeliusSecrets {
    pub api_key: String,
}

impl Secrets {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}

fn default_health_addr() -> SocketAddr {
    "0.0.0.0:8081".parse().unwrap()
}

fn default_graphql_addr() -> SocketAddr {
    "0.0.0.0:9002".parse().unwrap()
}

fn default_allowed_origins() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_db_pool_size() -> u32 {
    8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "solana-indexer-cfg-{}-{}.toml",
            std::process::id(),
            name
        ));
        std::fs::write(&p, body).unwrap();
        p
    }

    const BASE: &str = r#"
cluster = "devnet"
laserstream_endpoint = "https://laserstream-devnet-ewr.helius-rpc.com"
database_url = "postgresql://postgres:postgres@localhost:7654/solana_indexer"

[programs]
options_core  = "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t"
auction_venue = "8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk"
options_vault = "ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe"
"#;

    #[test]
    fn defaults_are_locked_down() {
        let p = write_tmp("defaults", BASE);
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.start_slot.is_none());
        assert_eq!(cfg.graphql_addr.port(), 9002);
        assert_eq!(cfg.health_addr.port(), 8081);
        assert_eq!(cfg.allowed_origins, vec!["*".to_string()]);
        assert!(!cfg.expose_playground);
        assert_eq!(cfg.db_pool_size, 8);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn start_slot_parses_when_set() {
        // Top-level key, so it must precede the [programs] table.
        let p = write_tmp(
            "start_slot",
            &BASE.replace("[programs]", "start_slot = 123456\n[programs]"),
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.start_slot, Some(123456));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn secrets_parse() {
        let p = write_tmp("secrets", "[helius]\napi_key = \"abc\"\n");
        let s = Secrets::load(&p).unwrap();
        assert_eq!(s.helius.api_key, "abc");
        std::fs::remove_file(&p).ok();
    }
}
