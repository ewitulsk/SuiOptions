use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;
use solana_tx::Network;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Public API bind address (proxied by nginx). Serves `/balance`,
    /// `/sponsor` and `/faucet`.
    pub bind_addr: SocketAddr,

    /// Solana cluster. Selects the RPC endpoint and the `[solana]` secret
    /// slot the station key is read from.
    pub network: Network,

    /// CORS allow-list. `["*"]` permits any origin.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,

    /// Below this balance (lamports) `/balance` reports unhealthy and
    /// `/sponsor` refuses with 503.
    pub min_balance_threshold_lamports: u64,

    /// Hard cap on the station's simulated lamport delta (fee + rent
    /// debits) for a single sponsored tx. Default 5_000_000 (0.005 SOL).
    #[serde(default = "default_max_sponsor")]
    pub max_sponsor_lamports_per_tx: u64,

    /// Enables `POST /faucet`. Force-disabled at boot when `network` is
    /// mainnet-beta regardless of this flag.
    #[serde(default)]
    pub faucet_enabled: bool,

    /// Per-request mint amount in RAW units, keyed by ticker. A test
    /// token without an entry here cannot be minted.
    #[serde(default)]
    pub faucet_amounts: BTreeMap<String, u64>,

    /// solana-token-info public base URL. The program ids it reports seed
    /// the sponsored-transaction templates built at boot; its testTokens
    /// block seeds the faucet.
    pub token_info_url: String,
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_max_sponsor() -> u64 {
    5_000_000
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
