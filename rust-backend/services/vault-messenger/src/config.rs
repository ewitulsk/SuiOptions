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

fn default_spoke_name() -> String {
    "robinhood".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub environment: String,
    /// Which coherent network bundle this profile carries: `testnet-set`
    /// or `mainnet-set` (plan §9). Informational + sanity-checked.
    pub network_set: String,
    pub bind_addr: SocketAddr,

    /// token-info base URL — the sole address source (multichain plan §9:
    /// ONE place to write, deployments.json, served by token-info).
    /// Every package/object id and spoke contract address below is
    /// resolved from `/package-info` at boot; the TOML fields exist only
    /// as break-glass overrides and win when explicitly set.
    pub token_info_url: String,

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
    /// and `endpoint_relayer`). Empty = resolved from token-info.
    #[serde(default)]
    pub trading_vault_pkg: String,
    /// The hub `TradingVault` shared object. A vault is runtime state,
    /// not a deployment artifact, so this one stays config.
    pub vault_id: String,
    /// Shared `VaultProtocolConfig`. Empty = resolved from token-info.
    #[serde(default)]
    pub protocol_config_id: String,
    /// Shared `EndpointRegistry` (endpoint.move). Empty = resolved from
    /// token-info (`multichain.endpointRegistryId`).
    #[serde(default)]
    pub endpoint_registry_id: String,
    /// Shared `OracleRegistry` (adapter pin). Empty = resolved from
    /// token-info.
    #[serde(default)]
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
    /// Spoke name in the deployment record (`multichain.spokes.<name>`).
    #[serde(default = "default_spoke_name")]
    pub name: String,
    /// EVM JSON-RPC endpoint (Robinhood chain). Runtime config, never a
    /// deployment artifact — always set here.
    pub rpc_url: String,
    /// EVM chain id, for transaction signing (NOT the protocol chain id —
    /// that one is baked into the wire envelopes by the contracts).
    /// 0 = resolved from token-info.
    #[serde(default)]
    pub chain_id: u64,
    /// Protocol spoke id — must match the hub binding and the SpokeVault's
    /// `SPOKE_ID`. 0 = resolved from token-info.
    #[serde(default)]
    pub spoke_id: u64,
    /// `SpokeVault` contract address. Empty = resolved from token-info.
    #[serde(default)]
    pub spoke_vault_address: String,
    /// The spoke's active `IMessageEndpoint` (dev: `RelayerEndpoint.sol`).
    /// Watched for `OutboundMessage(bytes)` and, on the dev-relayer
    /// transport, the target of `deliver(bytes)` submissions. Empty =
    /// resolved from token-info by `transport`.
    #[serde(default)]
    pub relayer_endpoint_address: String,
    /// Active transport for this lane. `dev-relayer` = this service
    /// submits hub→spoke deliveries itself; anything else (layerzero,
    /// ccip) = the transport delivers itself and we only confirm.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Sui-side pricing marker type for the spoke deposit/payout asset
    /// (the `M` bound at `bind_spoke`, e.g. `0x…::asset_markers::USDG`).
    /// Empty = derived from the resolved trading-vault package id.
    #[serde(default)]
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

    /// Fill every empty/zero id and address from the token-info snapshot
    /// (the served deployments.json record — multichain plan §9), then
    /// require completeness. Explicitly-set TOML values always win, so a
    /// config can still pin a single value as break-glass.
    pub fn resolve_from_token_info(&mut self, snap: &token_info_client::Snapshot) -> Result<()> {
        let pi = &snap.package_info;

        fn fill(slot: &mut String, value: Option<&str>) {
            if slot.is_empty() {
                if let Some(v) = value {
                    *slot = v.to_string();
                }
            }
        }
        fn fill_opt(slot: &mut Option<String>, value: Option<&str>) {
            if slot.is_none() {
                *slot = value.map(str::to_string);
            }
        }

        fill(
            &mut self.hub.trading_vault_pkg,
            pi.trading_vault.as_ref().map(|p| p.package_id.as_str()),
        );
        let tvo = pi.trading_vault_objects.as_ref();
        fill(
            &mut self.hub.protocol_config_id,
            tvo.map(|o| o.vault_protocol_config_id.as_str()),
        );
        fill(&mut self.hub.oracle_registry_id, tvo.map(|o| o.oracle_registry_id.as_str()));
        fill(
            &mut self.hub.endpoint_registry_id,
            pi.multichain.as_ref().map(|m| m.endpoint_registry_id.as_str()),
        );
        fill_opt(
            &mut self.hub.deepbook_adapter_pkg,
            pi.deepbook_adapter.as_ref().map(|p| p.package_id.as_str()),
        );
        fill_opt(
            &mut self.hub.options_adapter_pkg,
            pi.options_adapter.as_ref().map(|p| p.package_id.as_str()),
        );
        fill_opt(
            &mut self.hub.exchange_adapter_pkg,
            pi.exchange_adapter.as_ref().map(|p| p.package_id.as_str()),
        );
        fill_opt(
            &mut self.hub.equity_oracle_pkg,
            pi.equity_oracle.as_ref().map(|p| p.package_id.as_str()),
        );

        let spoke_rec = pi
            .multichain
            .as_ref()
            .and_then(|m| m.spokes.get(&self.spoke.name));
        if let Some(rec) = spoke_rec {
            if self.spoke.chain_id == 0 {
                self.spoke.chain_id = rec.evm_chain_id;
            }
            if self.spoke.spoke_id == 0 {
                self.spoke.spoke_id = rec.spoke_id;
            }
            fill(&mut self.spoke.spoke_vault_address, Some(rec.spoke_vault.as_str()));
            // The watched/submitted endpoint follows the active transport.
            let endpoint = match self.spoke.transport.as_str() {
                "layerzero" => rec.layerzero_endpoint.as_deref(),
                "ccip" => rec.ccip_endpoint.as_deref(),
                _ => rec.relayer_endpoint.as_deref(),
            };
            fill(&mut self.spoke.relayer_endpoint_address, endpoint);
            if self.spoke.start_block.is_none() {
                self.spoke.start_block = Some(rec.deploy_block);
            }
        }
        if self.spoke.asset_marker_type.is_empty() && !self.hub.trading_vault_pkg.is_empty() {
            self.spoke.asset_marker_type =
                format!("{}::asset_markers::USDG", self.hub.trading_vault_pkg);
        }

        let mut missing = vec![];
        if self.hub.trading_vault_pkg.is_empty() {
            missing.push("hub.trading_vault_pkg (token-info tradingVault)");
        }
        if self.hub.protocol_config_id.is_empty() {
            missing.push("hub.protocol_config_id (token-info tradingVaultObjects)");
        }
        if self.hub.oracle_registry_id.is_empty() {
            missing.push("hub.oracle_registry_id (token-info tradingVaultObjects)");
        }
        if self.hub.endpoint_registry_id.is_empty() {
            missing.push("hub.endpoint_registry_id (token-info multichain)");
        }
        if self.spoke.chain_id == 0 {
            missing.push("spoke.chain_id (token-info multichain.spokes)");
        }
        if self.spoke.spoke_id == 0 {
            missing.push("spoke.spoke_id (token-info multichain.spokes)");
        }
        if self.spoke.spoke_vault_address.is_empty() {
            missing.push("spoke.spoke_vault_address (token-info multichain.spokes)");
        }
        if self.spoke.relayer_endpoint_address.is_empty() {
            missing.push("spoke.relayer_endpoint_address (token-info multichain.spokes endpoint for the active transport)");
        }
        if !missing.is_empty() {
            bail!(
                "config unresolved after token-info overlay — set these in the TOML or run the \
                 corresponding deploy pass: {}",
                missing.join(", ")
            );
        }
        Ok(())
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

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn base_config() -> Config {
        let toml = r#"
            environment = "test"
            network_set = "testnet-set"
            bind_addr = "127.0.0.1:9021"
            token_info_url = "http://token-info:9005"
            database_url = "postgres://x"
            fee_pot_low_wei = "5000000000000000"

            [hub]
            network = "testnet"
            oracle_url = "http://oracle:9010"
            vault_id = "0xv"

            [spoke]
            rpc_url = "http://spoke:8545"
        "#;
        toml::from_str(toml).expect("parses with servable fields absent")
    }

    fn snapshot(with_multichain: bool) -> token_info_client::Snapshot {
        let mut package_info = serde_json::json!({
            "packageId": "0x1", "adminCapId": "0x2", "protocolConfigId": "0x3",
            "upgradeCapId": "0x4", "treasuryId": null, "publishDigest": "x",
            "initDigest": null, "deployer": "0x5", "deployedAt": "", "network": "testnet",
            "tradingVault": { "packageId": "0xtv", "upgradeCapId": "0x0", "publishDigest": "d", "deployedAt": "" },
            "tradingVaultObjects": {
                "vaultProtocolConfigId": "0xcfg", "integrationRegistryId": "0xir",
                "oracleRegistryId": "0xor", "pythFeedRegistryId": "0xpf",
                "poolAllowlistId": "0xpa", "activationDigest": "dig",
                "registrarPubkey": ""
            }
        });
        if with_multichain {
            package_info["multichain"] = serde_json::json!({
                "endpointRegistryId": "0xer",
                "hubChainId": 1,
                "spokes": {
                    "robinhood": {
                        "spokeId": 3, "protocolChainId": 257, "evmChainId": 46898,
                        "spokeVault": "0x00000000000000000000000000000000000000aa",
                        "relayerEndpoint": "0x00000000000000000000000000000000000000bb",
                        "usdg": { "address": "0x00000000000000000000000000000000000000cc", "decimals": 6, "assetCode": 1 },
                        "deployBlock": 4242, "deployer": "0xd", "deployedAt": ""
                    }
                }
            });
        }
        token_info_client::Snapshot {
            package_info: serde_json::from_value(package_info).unwrap(),
            tokens: vec![],
        }
    }

    #[test]
    fn overlay_fills_everything_from_token_info() {
        let mut cfg = base_config();
        cfg.resolve_from_token_info(&snapshot(true)).unwrap();
        assert_eq!(cfg.hub.trading_vault_pkg, "0xtv");
        assert_eq!(cfg.hub.protocol_config_id, "0xcfg");
        assert_eq!(cfg.hub.oracle_registry_id, "0xor");
        assert_eq!(cfg.hub.endpoint_registry_id, "0xer");
        assert_eq!(cfg.spoke.chain_id, 46898);
        assert_eq!(cfg.spoke.spoke_id, 3);
        assert_eq!(cfg.spoke.spoke_vault_address, "0x00000000000000000000000000000000000000aa");
        assert_eq!(
            cfg.spoke.relayer_endpoint_address,
            "0x00000000000000000000000000000000000000bb"
        );
        assert_eq!(cfg.spoke.start_block, Some(4242));
        assert_eq!(cfg.spoke.asset_marker_type, "0xtv::asset_markers::USDG");
    }

    #[test]
    fn explicit_toml_values_win_over_token_info() {
        let mut cfg = base_config();
        cfg.spoke.spoke_vault_address = "0xoverride".into();
        cfg.spoke.chain_id = 7;
        cfg.hub.trading_vault_pkg = "0xpin".into();
        cfg.resolve_from_token_info(&snapshot(true)).unwrap();
        assert_eq!(cfg.spoke.spoke_vault_address, "0xoverride");
        assert_eq!(cfg.spoke.chain_id, 7);
        assert_eq!(cfg.hub.trading_vault_pkg, "0xpin");
        assert_eq!(cfg.spoke.asset_marker_type, "0xpin::asset_markers::USDG");
    }

    #[test]
    fn missing_multichain_block_reports_what_is_unresolved() {
        let mut cfg = base_config();
        let err = cfg.resolve_from_token_info(&snapshot(false)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("endpoint_registry_id"), "{msg}");
        assert!(msg.contains("spoke_vault_address"), "{msg}");
    }

    #[test]
    fn layerzero_transport_resolves_that_endpoint() {
        let mut cfg = base_config();
        cfg.spoke.transport = "layerzero".into();
        // Record only carries a relayer endpoint → unresolved, named.
        let err = cfg.resolve_from_token_info(&snapshot(true)).unwrap_err();
        assert!(err.to_string().contains("relayer_endpoint_address"), "{err}");
    }
}
