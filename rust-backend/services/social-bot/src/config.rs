use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Bind address (proxied by nginx — Slack/Discord deliver webhooks here).
    pub bind_addr: SocketAddr,

    /// twitter-service base URL (internal, compose `net` network).
    pub twitter_service_url: String,

    /// Slack user ids (`U…`) allowed to run /tweet. Empty = nobody.
    #[serde(default)]
    pub slack_allowed_user_ids: Vec<String>,

    /// Discord user ids (snowflakes) allowed to run /tweet. Empty = nobody.
    #[serde(default)]
    pub discord_allowed_user_ids: Vec<String>,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
