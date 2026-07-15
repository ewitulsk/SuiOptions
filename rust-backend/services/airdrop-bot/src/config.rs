use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Bind address. Proxied by nginx at /<env>/airdrop-bot/ — Discord
    /// delivers signed interaction webhooks here.
    pub bind_addr: SocketAddr,

    /// engagement-service base URL (internal compose network).
    pub engagement_service_url: String,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
