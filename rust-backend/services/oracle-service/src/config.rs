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

    /// CORS allow-list for the browser-facing routes (descriptor, prices,
    /// legs — all read-only). `["*"]` permits any origin, mirroring
    /// token-info (SO-357). Without this layer every cross-origin fetch
    /// was CORS-blocked, which is what silently forced the frontend's
    /// compiled-Pyth fallback.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,
}

/// `[oracle]` — the provider switch plus its per-provider endpoints.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OracleConfig {
    /// `pyth` | `switchboard`. Defaults to Pyth, so a config that says
    /// nothing keeps behaving exactly as it did.
    #[serde(default)]
    pub provider: protocol_types::OracleProvider,

    /// Crossbar base URL for SIGNED quote bundles (`/v2/update`), used
    /// when `provider = "switchboard"`. Unset on a Pyth-only deployment.
    ///
    /// Still the PUBLIC instance (SO-346): our in-compose crossbar
    /// cannot serve Sui-testnet signed quotes — its Sui oracle-cache
    /// refresh ignores the `SUI_*_RPC` env overrides (verified SO-352:
    /// the routine still hits the hardcoded, deprecated public fullnode)
    /// so its cache stays empty and `/v2/update` 404s.
    #[serde(default)]
    pub crossbar_url: Option<String>,

    /// Crossbar base URL for the UNSIGNED price data plane
    /// (`/v2/simulate`, SO-353). Defaults to `crossbar_url` when unset.
    ///
    /// Split from `crossbar_url` because the two paths currently need
    /// different instances: simulate reads crossbar's own Surge stream
    /// (no Sui oracle cache), so our in-compose instance serves it fine —
    /// and polling every 1.5s would hit the public instance's rate
    /// limits.
    #[serde(default)]
    pub data_plane_crossbar_url: Option<String>,

    /// `network` query for Crossbar quote requests. Crossbar's signing
    /// set is per SOLANA cluster and defaults to mainnet; Sui testnet's
    /// queue is backed by Solana DEVNET, so this must be "devnet" there
    /// or every bundle is signed under the wrong queue.
    #[serde(default)]
    pub crossbar_network: Option<String>,

    /// Sui JSON-RPC used to resolve the queue's registered-oracle map
    /// (`Queue.existing_oracles` on chain). Required for switchboard.
    #[serde(default)]
    pub sui_rpc_url: Option<String>,

    /// The Sui `Queue` OBJECT `run_N` validates signing oracles against,
    /// read from the Switchboard `State` object for this network.
    #[serde(default)]
    pub switchboard_queue_id: Option<String>,

    /// That queue's 32-byte `queue_key`. Crossbar reports the queue its
    /// signatures were produced under; comparing the two catches a
    /// cross-queue bundle off chain instead of as an opaque `run_N`
    /// abort. The public Crossbar answers for a DIFFERENT queue than Sui
    /// testnet's, so this is a live failure mode, not a theoretical one.
    #[serde(default)]
    pub switchboard_queue_key: Option<String>,

    /// Switchboard's own `on_demand` package id (the package exposing
    /// `quote_submit_action::run_N`) — NOT our adapter. Served to
    /// PTB composers via `GET /oracle/legs` so nothing else pins it.
    /// Take it from the `published-at` of the branch our
    /// `contracts/oracle-switchboard/Move.toml` links, never from docs
    /// prose (see docs/oracle-abstraction-plan.md §2.5).
    #[serde(default)]
    pub switchboard_package_id: Option<String>,
}

fn default_hermes() -> String {
    "https://hermes.pyth.network".into()
}

fn default_benchmarks() -> String {
    "https://benchmarks.pyth.network".into()
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
