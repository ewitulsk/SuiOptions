//! Service config. Loaded via `runtime_config::config_load` so `${DB_HOST}` /
//! `${DB_PASSWORD}` expand from the environment at boot.
//!
//! Every network-dependent value lives here (multichain plan §9): the
//! `network_set` selector names which coherent bundle a profile carries
//! (testnet-set vs mainnet-set); promotion is a config/deploy change with
//! zero code edits.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

fn default_db_pool_size() -> u32 {
    4
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_evm_poll_interval_secs() -> u64 {
    10
}
fn default_hub_poll_interval_secs() -> u64 {
    10
}
fn default_deliver_interval_secs() -> u64 {
    5
}
fn default_state_sync_interval_secs() -> u64 {
    300
}
fn default_config_sync_interval_secs() -> u64 {
    900
}
fn default_alert_interval_secs() -> u64 {
    60
}
fn default_max_attempts() -> i32 {
    8
}
fn default_backoff_base_secs() -> u64 {
    10
}
fn default_backoff_cap_secs() -> u64 {
    600
}
fn default_queue_stalled_after_secs() -> i64 {
    900
}
fn default_payout_aged_after_secs() -> i64 {
    3600
}
fn default_sui_gas_budget() -> u64 {
    500_000_000
}
fn default_evm_gas_limit() -> u64 {
    1_000_000
}
fn default_max_scan_blocks() -> u64 {
    5_000
}
fn default_transport() -> String {
    "dev-relayer".to_string()
}
fn default_config_sync_event_types() -> Vec<String> {
    // Module-relative names; the watcher prefixes the trading-vault
    // package id. Any new event of one of these types triggers an
    // immediate ConfigSync push (hub pause / risk / identity changes).
    vec![
        "events::DepositsPaused".to_string(),
        "events::RiskStateChanged".to_string(),
        "events::CuratorRotated".to_string(),
        "events::VaultClosed".to_string(),
        "events::SpokeCuratorSet".to_string(),
        "events::SpokeIntegrationsRootSet".to_string(),
    ]
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub environment: String,
    /// Which coherent network bundle this profile carries: `testnet-set`
    /// or `mainnet-set` (plan §9). Informational + sanity-checked.
    pub network_set: String,
    pub bind_addr: SocketAddr,

    /// Shared RDS Postgres, assembled from `${DB_HOST}` / `${DB_PASSWORD}`.
    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,

    #[serde(default = "default_evm_poll_interval_secs")]
    pub evm_poll_interval_secs: u64,
    #[serde(default = "default_hub_poll_interval_secs")]
    pub hub_poll_interval_secs: u64,
    #[serde(default = "default_deliver_interval_secs")]
    pub deliver_interval_secs: u64,
    /// Spoke `syncState()` crank cadence (plan default: 5 min).
    #[serde(default = "default_state_sync_interval_secs")]
    pub state_sync_interval_secs: u64,
    /// Hub `build_config_sync` heartbeat cadence (plan default: 15 min);
    /// observed pause/risk events push one immediately.
    #[serde(default = "default_config_sync_interval_secs")]
    pub config_sync_interval_secs: u64,
    #[serde(default = "default_alert_interval_secs")]
    pub alert_interval_secs: u64,

    /// Delivery attempts per message before it is marked failed (alerts).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: i32,
    /// Capped exponential backoff between delivery attempts.
    #[serde(default = "default_backoff_base_secs")]
    pub backoff_base_secs: u64,
    #[serde(default = "default_backoff_cap_secs")]
    pub backoff_cap_secs: u64,

    /// Oldest-pending age that fires `vault-messenger-queue-stalled`.
    #[serde(default = "default_queue_stalled_after_secs")]
    pub queue_stalled_after_secs: i64,
    /// Unsettled-payable age that fires `vault-messenger-payout-queue-aged`.
    #[serde(default = "default_payout_aged_after_secs")]
    pub payout_aged_after_secs: i64,
    /// Fee-pot floor (spoke native wei, decimal string — u128 doesn't fit
    /// TOML integers) that fires `vault-messenger-fee-pot-low`.
    pub fee_pot_low_wei: String,

    pub hub: HubConfig,
    pub spoke: SpokeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HubConfig {
    /// `testnet` | `mainnet` — selects the relayer key slot and public RPC.
    pub network: sui_tx::sui_client::Network,
    /// oracle-service base URL — price legs for the appraisal-bearing
    /// handlers come from `/oracle/descriptor` + `/oracle/legs`, so the
    /// adapter package/entry names follow the live oracle pin.
    pub oracle_url: String,
    /// trading-vault-v2 package id (`vault_v2` modules incl. `multichain`
    /// and `endpoint_relayer`).
    pub trading_vault_pkg: String,
    /// The hub `TradingVault` shared object.
    pub vault_id: String,
    /// Shared `VaultProtocolConfig`.
    pub protocol_config_id: String,
    /// Shared `EndpointRegistry` (endpoint.move).
    pub endpoint_registry_id: String,
    /// Shared `OracleRegistry` (adapter pin).
    pub oracle_registry_id: String,
    /// Optional adapter packages for appraisal legs on vaults that hold
    /// more than cash (mirrors the keeper's `AppraisalRefs`).
    #[serde(default)]
    pub deepbook_adapter_pkg: Option<String>,
    #[serde(default)]
    pub options_adapter_pkg: Option<String>,
    #[serde(default)]
    pub exchange_adapter_pkg: Option<String>,
    #[serde(default)]
    pub equity_oracle_pkg: Option<String>,
    #[serde(default)]
    pub equity_book_id: Option<String>,
    #[serde(default)]
    pub vol_book_id: Option<String>,
    #[serde(default = "default_sui_gas_budget")]
    pub gas_budget: u64,
    /// Hub event types (module-relative, `events::Name`) that trigger an
    /// immediate ConfigSync push.
    #[serde(default = "default_config_sync_event_types")]
    pub config_sync_event_types: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpokeConfig {
    /// EVM JSON-RPC endpoint (Robinhood chain).
    pub rpc_url: String,
    /// EVM chain id, for transaction signing (NOT the protocol chain id —
    /// that one is baked into the wire envelopes by the contracts).
    pub chain_id: u64,
    /// Protocol spoke id — must match the hub binding and the SpokeVault's
    /// `SPOKE_ID`.
    pub spoke_id: u64,
    /// `SpokeVault` contract address.
    pub spoke_vault_address: String,
    /// The spoke's active `IMessageEndpoint` (dev: `RelayerEndpoint.sol`).
    /// Watched for `OutboundMessage(bytes)` and, on the dev-relayer
    /// transport, the target of `deliver(bytes)` submissions.
    pub relayer_endpoint_address: String,
    /// Active transport for this lane. `dev-relayer` = this service
    /// submits hub→spoke deliveries itself; anything else (layerzero,
    /// ccip) = the transport delivers itself and we only confirm.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Sui-side pricing marker type for the spoke deposit/payout asset
    /// (the `M` bound at `bind_spoke`, e.g. `0x…::asset_markers::USDG`).
    pub asset_marker_type: String,
    /// Spoke-local asset codes, for display/reporting only (the wire
    /// carries the codes; the hub binding owns the meaning).
    #[serde(default)]
    pub asset_codes: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_evm_gas_limit")]
    pub gas_limit: u64,
    /// Max blocks scanned per watcher tick.
    #[serde(default = "default_max_scan_blocks")]
    pub max_scan_blocks: u64,
    /// Optional first block to scan on a fresh DB (default: the chain tip
    /// at first boot).
    #[serde(default)]
    pub start_block: Option<u64>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let cfg: Self = runtime_config::config_load::load_toml(path)?;
        cfg.validate().with_context(|| format!("validating {path}"))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.network_set != "testnet-set" && self.network_set != "mainnet-set" {
            bail!("network_set must be 'testnet-set' or 'mainnet-set', got {}", self.network_set);
        }
        self.fee_pot_low_wei
            .parse::<u128>()
            .with_context(|| format!("fee_pot_low_wei is not a u128: {}", self.fee_pot_low_wei))?;
        Ok(())
    }

    pub fn fee_pot_low_wei(&self) -> u128 {
        // Checked in validate().
        self.fee_pot_low_wei.parse().expect("validated at load")
    }
}

/// The `[evm]` block of the rendered secrets TOML. `runtime_config::Secrets`
/// has no EVM slot, so the messenger reads its spoke key from the same file
/// through this local shape (unknown sections are ignored on both parses).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EvmSecrets {
    #[serde(default)]
    pub evm: EvmKey,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EvmKey {
    /// 32-byte secp256k1 key, hex (`0x` prefix optional).
    pub private_key: Option<String>,
}

impl EvmSecrets {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading secrets file {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing [evm] from {}", path.display()))
    }

    pub fn private_key(&self) -> Result<&str> {
        self.evm
            .private_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("secrets file is missing evm.private_key"))
    }
}
