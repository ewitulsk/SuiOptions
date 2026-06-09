use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;
use sui_tx::Network;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Public API bind address (proxied by nginx). Serves `/balance` +
    /// `/sponsor`.
    pub bind_addr: SocketAddr,

    /// Sui network. Selects the RPC endpoint and the `[sui]` secret slot the
    /// sponsor key is read from.
    pub network: Network,

    /// CORS allow-list. `["*"]` permits any origin.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,

    /// Below this balance (MIST) `/balance` reports unhealthy and the frontend
    /// defaults the sponsor toggle off.
    pub min_balance_threshold_mist: u64,

    /// Hard cap on a single sponsored gas budget (MIST). Also the dry-run
    /// budget. Default 0.5 SUI.
    #[serde(default = "default_max_budget")]
    pub max_gas_budget_mist: u64,

    /// Floor for a sponsored gas budget (MIST). Default 0.001 SUI.
    #[serde(default = "default_min_budget")]
    pub min_gas_budget_mist: u64,

    /// Safety margin added on top of the dry-run estimate, basis points.
    /// Default 2500 (+25%).
    #[serde(default = "default_buffer_bps")]
    pub gas_budget_buffer_bps: u64,

    /// Move packages the station will sponsor calls to (`0x`-prefixed ids).
    /// Empty = allow all (local dev only).
    #[serde(default)]
    pub allowed_packages: Vec<String>,
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_max_budget() -> u64 {
    500_000_000
}
fn default_min_budget() -> u64 {
    1_000_000
}
fn default_buffer_bps() -> u64 {
    2_500
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
