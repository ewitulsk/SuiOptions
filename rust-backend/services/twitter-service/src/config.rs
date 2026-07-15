use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Bind address. Internal-only — reachable on the compose `net` network,
    /// never proxied by nginx.
    pub bind_addr: SocketAddr,

    /// Twitter API base URL. Override in tests only.
    #[serde(default = "default_api_base")]
    pub twitter_api_base: String,
}

fn default_api_base() -> String {
    "https://api.twitter.com".to_string()
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
