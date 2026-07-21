use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;
use sui_tx::Network;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Deployment environment: `dev` / `staging` / `prod`. Logging only.
    pub environment: String,

    /// Public API bind address (proxied by nginx). Serves `/pubkey` +
    /// `/policy` + `/sign`.
    pub bind_addr: SocketAddr,

    /// Sui network. Selects the RPC endpoint and the `[sui]` secret slot the
    /// service signing key is read from.
    pub network: Network,

    /// CORS allow-list. `["*"]` permits any origin.
    #[serde(default = "default_cors")]
    pub allowed_origins: Vec<String>,

    /// token-info public base URL. Supplies the trading_vault package id the
    /// strict-tier `return_external` target is pinned against.
    pub token_info_url: String,

    /// Append-only JSONL audit log — one line per /sign request. Open
    /// failure at boot is fatal.
    pub audit_log_path: PathBuf,

    /// TOML file the per-vault FROST key shares are persisted in (the
    /// service half of each vault's 2-of-2 threshold-ed25519 parent key).
    /// Written by the /frost/keygen ceremony; loaded at boot. Lives NEXT TO
    /// secrets.toml in dev but NOT inside it: deployed secrets.toml is
    /// re-rendered from AWS Secrets Manager on every deploy, which would
    /// discard runtime-generated shares — so deployed envs point this at the
    /// persistent data volume instead.
    pub frost_shares_path: PathBuf,

    /// The vaults whose external accounts this service co-signs for.
    #[serde(default)]
    pub vaults: Vec<VaultConfig>,
}

/// Per-vault policy configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct VaultConfig {
    /// TradingVault shared-object id. Also the strict-tier sweep target
    /// address (`vault_address` = this id as an address).
    pub vault_id: String,

    /// The external account address (the 2-of-2 multisig). Every signed tx
    /// must have this as its sender.
    pub external_account: String,

    /// Address every strict-tier `TransferObjects` must pay. Equals
    /// `vault_id` interpreted as an address.
    pub vault_address: String,

    /// Curator's multisig member public key (base64, flag-prefixed).
    /// Informational only — recorded so operators can re-derive the
    /// multisig address; never used to verify anything.
    #[serde(default)]
    pub curator_pubkey_b64: Option<String>,

    /// Per-tx cap on the pure u64 amount of `borrow_base` / `borrow_quote`.
    pub max_borrow_amount: u64,

    /// Shared-object allowlist for the auto tier: the DeepBook pools the
    /// account may trade, PLUS the margin registry / margin pools / the
    /// account's own MarginManager — any shared object its perimeter txs
    /// legitimately touch. The clock (0x6) and the vault object itself are
    /// allowed implicitly. Unknown shared objects → deny.
    #[serde(default)]
    pub allowed_pools: Vec<String>,

    /// The canonical `deepbook_margin` package id. Third-party — NOT in
    /// deployments.json — so it is pinned here per vault.
    pub deepbook_margin_package: String,

    /// The curator's day-to-day trading wallet. The ONLY wallet a Bluefin
    /// `authorize_account` payload may authorize on the vault's parent
    /// account. Absent → every authorize payload is denied.
    #[serde(default)]
    pub curator_wallet: Option<String>,

    /// Bluefin-specific pins for the FROST payload policy.
    #[serde(default)]
    pub bluefin: Option<BluefinVaultConfig>,
}

/// Optional Bluefin object-id pins. When set, payloads naming a different
/// store are denied; when unset the corresponding check is skipped.
#[derive(Debug, Clone, Deserialize)]
pub struct BluefinVaultConfig {
    /// Bluefin internal data store id (`ids` field of authorize payloads).
    #[serde(default)]
    pub ids_id: Option<String>,
    /// Bluefin external data store id (`eds` field of withdraw payloads).
    #[serde(default)]
    pub eds_id: Option<String>,
}

fn default_cors() -> Vec<String> {
    vec!["*".to_string()]
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        config_load::load_toml(path)
    }
}
