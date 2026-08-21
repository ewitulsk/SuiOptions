//! Service config, loaded via `runtime_config::config_load` so `${DB_HOST}` /
//! `${DB_PASSWORD}` expand from the environment at boot.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

fn default_db_pool_size() -> u32 {
    4
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_invite_ttl() -> i64 {
    7 * 86_400
}
/// Sandbox refuses anything above $2.00. Enforcing it here turns an opaque
/// Dakota rejection into a clear message before we spend a round-trip.
fn default_max_amount_minor() -> i64 {
    200
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub environment: String,
    pub bind_addr: SocketAddr,

    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,

    pub dakota: DakotaConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DakotaConfig {
    /// `https://api.platform.sandbox.dakota.xyz` for sandbox.
    pub base_url: String,

    /// Ed25519 public key that signs webhook deliveries, hex. Environment
    /// specific — the sandbox and production keys differ, and using the wrong
    /// one rejects every delivery.
    pub webhook_public_key: String,

    /// Publicly reachable URL Dakota should deliver to. Registered by the admin
    /// `POST /admin/webhooks/register` route rather than at boot, so a restart
    /// does not churn targets.
    #[serde(default)]
    pub webhook_url: Option<String>,

    /// Hard ceiling on any single transfer, in minor units. Exists because the
    /// sandbox caps at $2.00; raising it will not lift Dakota's own limit.
    #[serde(default = "default_max_amount_minor")]
    pub max_amount_minor: i64,

    /// Networks we will send to Dakota. Sandbox rejects mainnet ids outright,
    /// so listing only testnets here turns a confusing downstream 400 into a
    /// local one — and stops a fat-fingered mainnet id ever leaving the box.
    #[serde(default)]
    pub allowed_networks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    /// auth-service's INTERNAL base url (`http://auth-service:9008`). Used both
    /// to verify tokens and to mint invites.
    pub internal_url: String,
    /// Lifetime of invites this service mints, seconds.
    #[serde(default = "default_invite_ttl")]
    pub invite_ttl_secs: i64,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        runtime_config::config_load::load_toml(path)
    }

    /// Whether `network` may be sent to Dakota. An empty allow-list permits
    /// everything, which is what local dev wants.
    pub fn network_allowed(&self, network: &str) -> bool {
        let list = &self.dakota.allowed_networks;
        list.is_empty() || list.iter().any(|n| n == network)
    }
}
