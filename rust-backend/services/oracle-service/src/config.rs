use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// REST + WS + `/health` + `/metrics` bind address. Internal only — not
    /// nginx-proxied.
    pub bind_addr: SocketAddr,

    /// token-info base URL — used once at boot to discover which Pyth feeds to
    /// subscribe to (every catalog token with a `pyth_feed_id`).
    pub token_info_url: String,

    /// Hermes base URL for the live SSE subscription. Testnet (staging/prod)
    /// uses `hermes-beta`; mainnet uses stable `hermes`.
    #[serde(default = "default_hermes")]
    pub hermes_url: String,

    /// Benchmarks base URL for realized-vol daily closes.
    #[serde(default = "default_benchmarks")]
    pub benchmarks_url: String,
}

fn default_hermes() -> String {
    "https://hermes.pyth.network".into()
}

fn default_benchmarks() -> String {
    "https://benchmarks.pyth.network".into()
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
