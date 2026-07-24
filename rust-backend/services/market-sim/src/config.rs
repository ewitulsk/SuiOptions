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

    /// Sui network. The sim HARD-gates on testnet (plus a faucet-bearing
    /// token catalog) regardless of `enabled`.
    pub network: Network,

    /// Health/metrics bind address (observability ops server; proxied by
    /// nginx for the deploy health gate).
    #[serde(default = "default_health_addr")]
    pub health_addr: SocketAddr,

    /// Master switch. Even when true the sim refuses non-testnet. When the
    /// gates fail the process parks with /health green instead of exiting —
    /// a disabled sim must not crash-loop or page.
    #[serde(default)]
    pub enabled: bool,

    /// Spot pairs to band, as "BASE/QUOTE" symbols (e.g. "TSUI/TUSDC"),
    /// resolved against the token-info catalog. Pools are created LAZILY on
    /// first liquidity deployment: looked up by PoolCreated event, created
    /// via create_permissionless_pool when missing (costs the vendored-DEEP
    /// `pool_creation_fee` from the service wallet — fund it or the pair is
    /// skipped with a loud warning).
    #[serde(default)]
    pub spot_pairs: Vec<String>,

    /// Banding pass cadence.
    #[serde(default = "default_spot_interval_secs")]
    pub spot_interval_secs: u64,

    /// Half-band around the Pyth cross, bps.
    #[serde(default = "default_spot_band_bps")]
    pub spot_band_bps: u64,

    /// Per-side size as settlement notional (atomic units).
    #[serde(default = "default_spot_notional_per_side")]
    pub spot_notional_per_side: u64,

    /// The maker's BalanceManager; empty = create at boot and log (persist
    /// the logged id here — see the mm-bot state-file lesson: paths never
    /// resolve in-container, config pinning does).
    #[serde(default)]
    pub spot_balance_manager_id: Option<String>,

    #[serde(default = "default_gas_budget")]
    pub gas_budget: u64,

    /// Skip a banding pass if our last observation of either price is older
    /// than this.
    #[serde(default = "default_max_price_age_ms")]
    pub max_price_age_ms: u64,
    /// Skip a banding pass if Pyth's publisher timestamp is older than this.
    #[serde(default = "default_max_publish_lag_ms")]
    pub max_publish_lag_ms: u64,
    /// Skip a banding pass if either feed's Pyth confidence interval exceeds
    /// this many basis points of its price. 0 disables.
    #[serde(default)]
    pub max_conf_bps: u64,
}

fn default_health_addr() -> SocketAddr {
    "0.0.0.0:9018".parse().unwrap()
}
fn default_spot_interval_secs() -> u64 {
    60
}
fn default_spot_band_bps() -> u64 {
    200
}
fn default_spot_notional_per_side() -> u64 {
    100_000_000
}
fn default_gas_budget() -> u64 {
    100_000_000
}
fn default_max_price_age_ms() -> u64 {
    5_000
}
fn default_max_publish_lag_ms() -> u64 {
    10_000
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
