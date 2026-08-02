//! Indexer configuration.
//!
//! Loaded from a TOML file (default `config/config.toml`). The package id
//! is **not** in this file — it's fetched at runtime from the token-info
//! service, so re-deploying the contracts doesn't require editing the
//! indexer config.
//!
//! Pattern is borrowed from Pismo's indexer.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use runtime_config::config_load;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Network identifier. Accepted values: `mainnet`, `testnet`, `devnet`
    /// (case-insensitive). Used to derive the default JSON-RPC URL.
    pub network: String,

    /// token-info public base URL. The deployed `package_id` is fetched
    /// from here at boot (replaces reading `deployments.json`).
    pub token_info_url: String,

    /// Sui checkpoint remote store. Production-facing values:
    ///   `https://checkpoints.testnet.sui.io`
    ///   `https://checkpoints.mainnet.sui.io`
    pub remote_store_url: String,

    /// gRPC endpoint for the same network. Used only to discover the
    /// latest checkpoint when `start_checkpoint` is unset, and for the
    /// `/progress` tip poll. If omitted it is derived from `network`.
    /// `rpc_url` is accepted as a deprecated alias so an un-migrated config
    /// keeps loading.
    #[serde(default, alias = "rpc_url")]
    pub grpc_url: Option<String>,

    /// Checkpoint sequence number to start ingesting from. `0` is genesis;
    /// any non-zero value resumes from that checkpoint. **Unset** means
    /// "start from the current tip" — the indexer queries
    /// `sui_getLatestCheckpointSequenceNumber` at boot and uses that.
    #[serde(default)]
    pub start_checkpoint: Option<u64>,

    /// How many checkpoints the framework processes in parallel.
    pub concurrency: usize,

    /// Bind address for the GraphQL query API (SO-97). Consumers read protocol
    /// state just-in-time from `POST /graphql` here.
    #[serde(default = "default_graphql_addr")]
    pub graphql_addr: SocketAddr,

    /// CORS allow-list for the GraphQL API. `["*"]` (the default) allows any
    /// origin — matches the other public services (api-service, token-info).
    /// Set explicit origins to scope it.
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,

    /// Serve the GraphiQL playground at `GET /graphql` and leave schema
    /// introspection enabled. Defaults to `false` (locked down); dev/staging
    /// opt in via their config. Keep this off in prod.
    #[serde(default)]
    pub expose_playground: bool,

    /// Postgres connection string for the persistence layer. Standard libpq
    /// URL form, e.g. `postgresql://postgres:postgres@localhost:7654/indexer`.
    pub database_url: String,

    /// HTTP health-check bind address. The uptime dashboard and ALB target
    /// groups hit `GET /health` here. Defaults to `0.0.0.0:8081`.
    #[serde(default = "default_health_addr")]
    pub health_addr: SocketAddr,

    /// r2d2 pool size for Postgres connections. The worker holds at most one
    /// connection at a time; GraphQL query resolvers can hold additional ones
    /// briefly.
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }

    /// gRPC endpoint to query for the latest checkpoint. Returns the
    /// explicit `grpc_url` if set, else the public Sui endpoint for
    /// `network`.
    ///
    /// This is ONLY used for the boot tip and the `/progress` tip poll —
    /// checkpoint ingestion runs off `remote_store_url` through
    /// `sui-data-ingestion-core` and was never affected by the JSON-RPC
    /// deactivation.
    pub fn resolve_grpc_url(&self) -> Result<String> {
        if let Some(u) = &self.grpc_url {
            return Ok(u.clone());
        }
        Ok(match self.network.to_ascii_lowercase().as_str() {
            "mainnet" => sui_tx::Network::Mainnet.grpc_url().to_string(),
            "testnet" => sui_tx::Network::Testnet.grpc_url().to_string(),
            "devnet" => sui_tx::Network::Devnet.grpc_url().to_string(),
            other => {
                return Err(anyhow::anyhow!(
                    "no default endpoint for network {other}; set `grpc_url` explicitly"
                ));
            }
        })
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
        let p =
            std::env::temp_dir().join(format!("indexer-cfg-{}-{}.toml", std::process::id(), name));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn start_checkpoint_is_optional() {
        let p = write_tmp(
            "no_start",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.start_checkpoint.is_none());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn start_checkpoint_parses_when_set() {
        let p = write_tmp(
            "with_start",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
start_checkpoint = 12345
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.start_checkpoint, Some(12345));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn grpc_url_defaults_per_network() {
        let p = write_tmp(
            "rpc_default",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.resolve_grpc_url().unwrap().contains("testnet"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cors_and_playground_have_locked_down_defaults() {
        let p = write_tmp(
            "cors_defaults",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.allowed_origins, vec!["*".to_string()]);
        assert!(!cfg.expose_playground);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn cors_and_playground_parse_when_set() {
        let p = write_tmp(
            "cors_set",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
allowed_origins = ["http://localhost:5173"]
expose_playground = true
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.allowed_origins, vec!["http://localhost:5173".to_string()]);
        assert!(cfg.expose_playground);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn grpc_url_explicit_overrides_default() {
        let p = write_tmp(
            "rpc_explicit",
            r#"
network = "testnet"
token_info_url = "http://127.0.0.1:9005"
remote_store_url = "https://checkpoints.testnet.sui.io"
grpc_url = "https://my-private-fullnode.example.com:443"
concurrency = 5
database_url = "postgresql://postgres:postgres@localhost:7654/indexer"
"#,
        );
        let cfg = Config::load(&p).unwrap();
        assert_eq!(
            cfg.resolve_grpc_url().unwrap(),
            "https://my-private-fullnode.example.com:443"
        );
        std::fs::remove_file(&p).ok();
    }
}
