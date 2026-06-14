use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// `/health` + `/metrics` bind (Prometheus scrapes this).
    pub ops_addr: SocketAddr,

    /// Read-API bind (`GET /vault-apy/:vault_id`) — api-service calls this.
    pub read_addr: SocketAddr,

    /// GraphQL endpoint of the indexer inside the compose stack.
    pub indexer_graphql_url: String,

    /// token-info base URL (token catalog: decimals + pyth feed ids).
    pub token_info_url: String,

    /// Postgres URL for the predicted-APY DB. `${DB_PASSWORD}`/`${DB_HOST}`
    /// are injected by compose env and expanded at load time.
    pub database_url: String,

    /// Seconds between recompute ticks.
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    #[serde(default)]
    pub pyth: PythConfig,

    #[serde(default)]
    pub model: ModelConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythConfig {
    /// Pyth Hermes root (latest prices → spot cross).
    #[serde(default = "default_hermes")]
    pub hermes_url: String,
    /// Pyth Benchmarks root (historical closes → realized vol).
    #[serde(default = "default_benchmarks")]
    pub benchmarks_url: String,
    /// Trailing window (days) for realized vol.
    #[serde(default = "default_vol_window_days")]
    pub vol_window_days: u32,
    /// Reject a spot leg older than this many ms.
    #[serde(default = "default_max_publish_lag_ms")]
    pub max_publish_lag_ms: u64,
    /// Reject a spot leg whose conf/price exceeds this (bps).
    #[serde(default = "default_max_conf_bps")]
    pub max_conf_bps: u32,
}

impl Default for PythConfig {
    fn default() -> Self {
        Self {
            hermes_url: default_hermes(),
            benchmarks_url: default_benchmarks(),
            vol_window_days: default_vol_window_days(),
            max_publish_lag_ms: default_max_publish_lag_ms(),
            max_conf_bps: default_max_conf_bps(),
        }
    }
}

/// Pricing/forecast knobs. The fee + round-cadence values are TEMPORARY
/// defaults: they live in the on-chain `VaultConfig`, which has no endpoint
/// yet (see the api-service vault-config gap). When that endpoint ships, read
/// these per-vault instead of from config. `round_ms` is already derived from
/// the observed finalize spacing when a vault has ≥2 finalized rounds; this is
/// only the fallback.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// Number of future rounds to forecast (Tier 2 horizon, K).
    #[serde(default = "default_horizon")]
    pub forecast_horizon: u32,
    /// Keeper's delta-target snap (0.10 = 10-delta).
    #[serde(default = "default_delta")]
    pub delta_target: f64,
    /// Fallback round length when finalize spacing can't be derived.
    #[serde(default = "default_round_ms")]
    pub default_round_ms: u64,
    /// MOCK until the VaultConfig endpoint exists: annualized management fee.
    #[serde(default = "default_mgmt_bps")]
    pub mgmt_fee_bps_annual: u64,
    /// MOCK until the VaultConfig endpoint exists: performance fee on premium.
    #[serde(default = "default_perf_bps")]
    pub perf_fee_bps: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            forecast_horizon: default_horizon(),
            delta_target: default_delta(),
            default_round_ms: default_round_ms(),
            mgmt_fee_bps_annual: default_mgmt_bps(),
            perf_fee_bps: default_perf_bps(),
        }
    }
}

fn default_tick_secs() -> u64 {
    120
}
fn default_hermes() -> String {
    "https://hermes.pyth.network".into()
}
fn default_benchmarks() -> String {
    "https://benchmarks.pyth.network".into()
}
fn default_vol_window_days() -> u32 {
    30
}
fn default_max_publish_lag_ms() -> u64 {
    30_000
}
fn default_max_conf_bps() -> u32 {
    100
}
fn default_horizon() -> u32 {
    4
}
fn default_delta() -> f64 {
    0.10
}
fn default_round_ms() -> u64 {
    604_800_000
}
fn default_mgmt_bps() -> u64 {
    200
}
fn default_perf_bps() -> u64 {
    1000
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let cfg: Self = config_load::load_toml(path)?;
        Ok(cfg)
    }
}
