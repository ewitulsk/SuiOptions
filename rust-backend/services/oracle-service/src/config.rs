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

    /// token-info base URL — used once at boot to discover which feeds to
    /// subscribe to (every catalog token carrying a feed key for the live
    /// provider).
    pub token_info_url: String,

    /// **The oracle switch (SO-335).**
    ///
    /// This one field decides which provider the whole stack runs on:
    /// oracle-service subscribes to that provider's feeds, and serves it on
    /// `/oracle/descriptor` so the PTB composers (Rust and browser) build
    /// that provider's price legs. Nothing else needs redeploying to switch.
    ///
    /// Both adapters are published and allowlisted on chain, so flipping
    /// this and restarting oracle-service is the entire switch. Order
    /// matters when RETIRING a provider: allow -> verify -> disallow, never
    /// the reverse (see docs/oracle-abstraction-plan.md §4).
    #[serde(default)]
    pub oracle: OracleConfig,

    /// Hermes base URL for the live SSE subscription. Testnet (staging/prod)
    /// uses `hermes-beta`; mainnet uses stable `hermes`.
    #[serde(default = "default_hermes")]
    pub hermes_url: String,

    /// Benchmarks base URL for realized-vol daily closes.
    #[serde(default = "default_benchmarks")]
    pub benchmarks_url: String,
}

/// `[oracle]` — the provider switch plus its per-provider endpoints.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OracleConfig {
    /// `pyth` | `switchboard`. Defaults to Pyth, so a config that says
    /// nothing keeps behaving exactly as it did.
    #[serde(default)]
    pub provider: protocol_types::OracleProvider,

    /// Self-hosted Crossbar base URL, used when `provider = "switchboard"`
    /// to resolve feeds and fetch signed quote bundles. Unset on a
    /// Pyth-only deployment.
    #[serde(default)]
    pub crossbar_url: Option<String>,
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
