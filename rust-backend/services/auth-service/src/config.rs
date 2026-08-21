use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Public API bind address (proxied by nginx). Serves `/challenge`,
    /// `/login`, `/refresh`.
    pub public_bind_addr: SocketAddr,

    /// Internal API bind address. NEVER proxied — reachable only
    /// container-to-container / over the VPN. Serves `/verify`.
    pub internal_bind_addr: SocketAddr,

    /// CORS allow-list for the public API. `["*"]` permits any origin.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,

    /// Admin allowlist: Sui addresses permitted to obtain a JWT. `0x`-prefixed;
    /// case/padding-insensitive (normalized before comparison).
    #[serde(default)]
    pub admin_addresses: Vec<String>,

    /// token-info public base URL, for resolving the core package id behind
    /// the on-chain AdminCap login fallback (SO-422). Both this and
    /// `sui_graphql_url` must be set for the fallback to be active; unset
    /// (dev default) means the static list is the only check.
    #[serde(default)]
    pub token_info_url: Option<String>,

    /// Sui GraphQL endpoint for the owned-AdminCap ownership query (SO-422).
    #[serde(default)]
    pub sui_graphql_url: Option<String>,

    /// Issued-JWT lifetime, seconds. Default 1h.
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,

    /// Max age (from issue) a token may still be refreshed at, seconds.
    /// Bounds a session even though each access token is short-lived.
    /// Default 24h.
    #[serde(default = "default_refresh_max")]
    pub refresh_max_secs: u64,

    /// How long an issued login challenge stays valid, seconds. Default 5m.
    #[serde(default = "default_challenge_ttl")]
    pub challenge_ttl_secs: u64,
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_token_ttl() -> u64 {
    3600
}
fn default_refresh_max() -> u64 {
    86_400
}
fn default_challenge_ttl() -> u64 {
    300
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
