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
fn default_mid_sample_interval_secs() -> u64 {
    10
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_apy_tick_secs() -> u64 {
    120
}
fn default_oracle_url() -> String {
    "http://oracle-service:9013".into()
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
fn default_apy_cap() -> f64 {
    5.0 // clamp annualized APY to ±500% so a short round can't blow up
}
fn default_assumed_vrp() -> f64 {
    0.04 // mid variance-risk-premium: implied = realized + 4 vol points
}
fn default_vrp_band() -> f64 {
    0.02 // range spans assumed_vrp ± this (→ +2…+6 vol points)
}
fn default_vol_band() -> f64 {
    0.25 // Tier-1 range brackets realized vol by ±25%
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
    /// Indexer GraphQL endpoint — the apy sampler lists vaults / rounds /
    /// RFQs and pulls the realized-APY series from here.
    pub indexer_graphql_url: String,
    /// oracle-service base URL — the apy sampler's source for spot + realized vol.
    #[serde(default = "default_oracle_url")]
    pub oracle_url: String,
    /// Explicit fullnode override; defaults from `network`.
    #[serde(default)]
    pub rpc_url: Option<String>,

    #[serde(default = "default_discovery_interval_secs")]
    pub discovery_interval_secs: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// How often the order-book midpoint is sampled per watched pool.
    #[serde(default = "default_mid_sample_interval_secs")]
    pub mid_sample_interval_secs: u64,
    /// Trades for pools that left the tradeable set are dropped once their
    /// newest fill is older than this.
    #[serde(default = "default_ttl_hours")]
    pub ttl_hours: i64,

    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,

    /// Seconds between vault-APY recompute ticks (predicted + realized).
    #[serde(default = "default_apy_tick_secs")]
    pub apy_tick_secs: u64,
    #[serde(default)]
    pub pyth: PythConfig,
    #[serde(default)]
    pub model: ModelConfig,
}

/// Realized-vol + spot-guard knobs for the apy sampler. Prices and vol now come
/// from oracle-service; these only tune the consumer-side guards and window.
#[derive(Debug, Clone, Deserialize)]
pub struct PythConfig {
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
            vol_window_days: default_vol_window_days(),
            max_publish_lag_ms: default_max_publish_lag_ms(),
            max_conf_bps: default_max_conf_bps(),
        }
    }
}

/// Forecast/pricing knobs for the apy sampler. Fees, strike band, and round
/// cadence are NOT here — they're the vault's on-chain `VaultConfig`, read
/// per-vault from the served config. These two are genuine model knobs.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// Number of future rounds to forecast (Tier 2 horizon, K).
    #[serde(default = "default_horizon")]
    pub forecast_horizon: u32,
    /// Strike the Tier-2 forecast prices, as a call delta (~0.10 per doc 04).
    #[serde(default = "default_delta")]
    pub delta_target: f64,
    /// Clamp annualized APY to ±this fraction (e.g. 5.0 = ±500%). Stops a
    /// short-cadence (hourly) round's tiny premium from annualizing to absurd
    /// numbers.
    #[serde(default = "default_apy_cap")]
    pub apy_cap: f64,
    /// Assumed variance risk premium in vol points: the predicted edge comes
    /// from selling implied vol richer than realized (`implied = realized +
    /// assumed_vrp`). At 0 the premium exactly offsets expected assignment and
    /// the net APY is ≈ −fees (no free lunch).
    #[serde(default = "default_assumed_vrp")]
    pub assumed_vrp: f64,
    /// Tier-2 range half-width, in vol points, around `assumed_vrp`.
    #[serde(default = "default_vrp_band")]
    pub vrp_band: f64,
    /// Tier-1 range half-width as a fraction of realized vol (σ bracket).
    #[serde(default = "default_vol_band")]
    pub vol_band: f64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            forecast_horizon: default_horizon(),
            delta_target: default_delta(),
            apy_cap: default_apy_cap(),
            assumed_vrp: default_assumed_vrp(),
            vrp_band: default_vrp_band(),
            vol_band: default_vol_band(),
        }
    }
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
