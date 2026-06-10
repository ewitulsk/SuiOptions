//! Service config. Loaded via `runtime_config::config_load` so
//! `${CHART_DATABASE_URL}` expands from the environment at boot (the URL is
//! a secret — it never appears in the TOML itself).

use std::net::SocketAddr;

use anyhow::Result;
use serde::Deserialize;

fn default_db_pool_size() -> u32 {
    4
}
fn default_discovery_interval_secs() -> u64 {
    15
}
fn default_poll_interval_ms() -> u64 {
    2_000
}
fn default_ttl_hours() -> i64 {
    168 // 7 days
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub environment: String,
    /// Sui network — resolves the fullnode RPC for event polling.
    pub network: String,
    pub bind_addr: SocketAddr,

    /// Tiger Data TimescaleDB URL, injected as `${CHART_DATABASE_URL}`.
    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    /// Discovery source for tradeable buckets/pools.
    pub api_service_url: String,
    /// DeepBook ids (original package id for the OrderFilled filter).
    pub token_info_url: String,
    /// Explicit fullnode override; defaults from `network`.
    #[serde(default)]
    pub rpc_url: Option<String>,

    #[serde(default = "default_discovery_interval_secs")]
    pub discovery_interval_secs: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Trades for pools that left the tradeable set are dropped once their
    /// newest fill is older than this.
    #[serde(default = "default_ttl_hours")]
    pub ttl_hours: i64,

    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        runtime_config::config_load::load_toml(path)
    }

    pub fn resolve_rpc_url(&self) -> Result<String> {
        if let Some(u) = &self.rpc_url {
            return Ok(u.clone());
        }
        match self.network.to_ascii_lowercase().as_str() {
            "mainnet" => Ok(sui_sdk::SUI_MAINNET_URL.to_string()),
            "testnet" => Ok(sui_sdk::SUI_TESTNET_URL.to_string()),
            "devnet" => Ok(sui_sdk::SUI_DEVNET_URL.to_string()),
            other => anyhow::bail!("unknown network {other} and no rpc_url set"),
        }
    }
}
