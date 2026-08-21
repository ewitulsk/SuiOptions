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

    /// Postgres connection string for the identity store, assembled from
    /// `${DB_HOST}` / `${DB_PASSWORD}` at load time.
    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    /// Admin bootstrap: Sui addresses auto-provisioned with the `admin` role on
    /// first wallet login. `0x`-prefixed; case/padding-insensitive (normalized
    /// before comparison).
    ///
    /// This is the only account-creation path that does not require an invite,
    /// and it grants `admin` — treat it as the root-of-trust list.
    #[serde(default)]
    pub admin_addresses: Vec<String>,

    /// Default lifetime of a minted invite, seconds. Default 7 days — long
    /// enough to send a link and have a human act on it.
    #[serde(default = "default_invite_ttl")]
    pub invite_ttl_secs: i64,

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
fn default_db_pool_size() -> u32 {
    4
}
fn default_invite_ttl() -> i64 {
    7 * 86_400
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
