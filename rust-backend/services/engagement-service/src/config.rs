//! Service config. Loaded via `runtime_config::config_load` so `${DB_HOST}` /
//! `${DB_PASSWORD}` expand from the environment at boot.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

use crate::points::PointsConfig;

fn default_db_pool_size() -> u32 {
    4
}
fn default_poll_interval_secs() -> u64 {
    300
}
fn default_refresh_max_age_hours() -> i64 {
    168
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Bind address. Internal-only — reachable on the compose `net` network,
    /// never proxied by nginx.
    pub bind_addr: SocketAddr,

    /// Shared RDS Postgres, assembled from `${DB_HOST}` / `${DB_PASSWORD}`.
    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    pub twitter_service_url: String,
    /// twitter-service account name whose mentions accrue points. The name
    /// doubles as the searched @handle (twitter-service secrets name
    /// accounts by handle).
    pub twitter_account: String,

    /// Seconds between poll ticks (mention search + metrics refresh).
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Stop refreshing a tweet's counters once it is older than this —
    /// engagement on week-old tweets has flattened, and recent search only
    /// covers 7 days anyway.
    #[serde(default = "default_refresh_max_age_hours")]
    pub refresh_max_age_hours: i64,

    /// Engagement→airdrop-point conversion weights.
    pub points: PointsConfig,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
