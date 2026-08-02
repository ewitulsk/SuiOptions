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

    /// token-info public base URL. The protocol package it reports (plus, on
    /// dev/staging, the test-token packages) seeds the sponsored-PTB templates
    /// built at boot.
    pub token_info_url: String,

    /// Circle TokenMessengerMinter package id (per network) — enables
    /// sponsoring the CCTP bridge burn PTB, which calls Circle's
    /// `deposit_for_burn` directly. Unset where the bridge isn't offered.
    #[serde(default)]
    pub cctp_token_messenger_package: Option<String>,

    /// Pyth + Wormhole (latest upgraded) package ids for this network —
    /// enables sponsoring the Pyth price-update prefix legs on
    /// attestation-bearing trading-vault deposits. Unset leaves those
    /// deposits unsponsorable.
    #[serde(default)]
    pub pyth: Option<PythConfig>,

    /// Switchboard's own `on_demand` package (SO-335), needed to
    /// allowlist the quote-submit prefix on attestation-bearing
    /// deposits. Our adapter's id comes from token-info; only
    /// Switchboard's does not, because it is a third-party deployment we
    /// do not publish. Unset leaves Switchboard deposits unsponsorable.
    #[serde(default)]
    pub switchboard: Option<SwitchboardConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwitchboardConfig {
    pub package_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythConfig {
    pub pyth_package_id: String,
    pub wormhole_package_id: String,
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
