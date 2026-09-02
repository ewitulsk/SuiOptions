//! TOML config for the keeper.
//!
//! Vaults are **discovered**, not configured: the tick loop reads the
//! indexer's `trading_vaults` view and resolves the Pyth
//! `PriceInfoObject`s for the token catalog's feeds from the Pyth state
//! table (`src/discovery.rs`). The config carries only the endpoints,
//! the Pyth deployment handles, and the defaults applied to every
//! discovered vault.
//!
//! ```toml
//! indexer_graphql_url = "http://127.0.0.1:9002/graphql"
//! tick_secs = 15
//!
//! [pyth]
//! hermes_url          = "https://hermes-beta.pyth.network"  # testnet = beta feeds
//! pyth_package_id     = "0x…"   # latest (upgraded) pyth package
//! wormhole_package_id = "0x…"
//! pyth_state_id       = "0x…"
//! wormhole_state_id   = "0x…"
//!
//! [vault_defaults]
//! vol_window_days = 30
//! ```

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

fn default_health_addr() -> SocketAddr {
    "0.0.0.0:8086".parse().unwrap()
}
fn default_tick_secs() -> u64 {
    15
}
fn default_mark_refresh_interval_ms() -> u64 {
    300_000
}
fn default_hermes_url() -> String {
    "https://hermes.pyth.network".into()
}
fn default_update_fee_mist() -> u64 {
    1
}
fn default_vol_window_days() -> u32 {
    30
}
fn default_reconciliation_tolerance_bps() -> u64 {
    2_000
}
fn default_equity_stale_alert_ms() -> u64 {
    3_600_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeeperConfig {
    #[serde(default = "default_health_addr")]
    pub health_addr: SocketAddr,

    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    /// Minimum spacing between per-vault mark-refresh appraisals (crank
    /// 8). Every mark is a paid on-chain tx per vault, so this is the
    /// main gas-vs-freshness knob: 5 min default; staging runs slower
    /// (SO-346 — appraisal spam drained the shared wallet).
    #[serde(default = "default_mark_refresh_interval_ms")]
    pub mark_refresh_interval_ms: u64,

    /// Indexer GraphQL endpoint — trading-vault discovery
    /// (`trading_vaults` view) and option-bucket lookups.
    pub indexer_graphql_url: String,

    pub pyth: PythKeeperConfig,

    /// Defaults applied to every discovered vault.
    #[serde(default)]
    pub vault_defaults: VaultDefaults,

    /// External-account (SO-299) knobs: reconciliation alert thresholds
    /// and the operator/testing equity-post map.
    #[serde(default)]
    pub external: ExternalConfig,
}

/// External-account monitoring + equity-poster knobs (SO-299). All
/// defaulted — the section may be absent entirely.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExternalConfig {
    /// Alert (`hedge-reconciliation`) when attested equity deviates from
    /// recorded exposure by more than this many bps of exposure, in
    /// either direction.
    pub reconciliation_tolerance_bps: u64,

    /// Alert when exposure > 0 but the equity mark is missing or older
    /// than this.
    pub equity_stale_alert_ms: u64,

    /// Operator/testing equity source for the poster crank: vault id
    /// (`"0x…"`) → target equity in deposit-asset units. Overridden by the
    /// Bluefin reader below; empty ⇒ posting disabled.
    pub equity_posts: std::collections::BTreeMap<String, u64>,

    /// Bluefin venue equity reader (SO-305): polls the venue's public
    /// account endpoint for each vault's FROST parent account and feeds the
    /// equity-poster crank through `venue_equity::Bluefin`. Absent ⇒ the
    /// reader is off (`equity_posts` / `Disabled` behavior unchanged).
    pub bluefin: Option<BluefinEquityConfig>,
}

/// `[external.bluefin]`: the venue environment + per-vault parent accounts.
#[derive(Debug, Clone, Deserialize)]
pub struct BluefinEquityConfig {
    /// Bluefin account-data host, e.g. `https://api.sui-staging.bluefin.io`
    /// (their Sui-testnet env) or `https://api.sui-prod.bluefin.io`.
    pub base_url: String,

    /// Account-endpoint poll cadence. The response is server-cached ~5s;
    /// polling faster buys nothing.
    #[serde(default = "default_bluefin_poll_interval_ms")]
    pub poll_interval_ms: u64,

    /// A cached mark older than this yields no opinion (the crank's
    /// `equity_stale_alert_ms` alerting then surfaces the gap).
    #[serde(default = "default_bluefin_max_age_ms")]
    pub max_age_ms: u64,

    /// Vault id (`"0x…"`) → the vault's Bluefin parent-account identity.
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, BluefinAccountConfig>,
}

/// One `[external.bluefin.accounts."0x…"]` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct BluefinAccountConfig {
    /// The FROST parent account address (must equal the vault's registered
    /// external account, or the reader posts nothing).
    pub account: String,
    /// Deposit-asset decimals for scaling Bluefin's E9 values (USDC = 6).
    #[serde(default = "default_bluefin_asset_decimals")]
    pub asset_decimals: u8,
}

fn default_bluefin_poll_interval_ms() -> u64 {
    10_000
}

fn default_bluefin_max_age_ms() -> u64 {
    60_000
}

fn default_bluefin_asset_decimals() -> u8 {
    6
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            reconciliation_tolerance_bps: default_reconciliation_tolerance_bps(),
            equity_stale_alert_ms: default_equity_stale_alert_ms(),
            equity_posts: Default::default(),
            bluefin: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythKeeperConfig {
    /// Hermes endpoint serving the SAME feed set the network's
    /// PriceInfoObjects are keyed by: `hermes-beta.pyth.network` for Sui
    /// testnet (beta feed ids), `hermes.pyth.network` for mainnet. Used only
    /// for the on-chain VAA (trading_vault.rs); spot/σ come from oracle-service.
    #[serde(default = "default_hermes_url")]
    pub hermes_url: String,

    /// Latest (post-upgrade) Pyth package — the address the
    /// `pyth::pyth` entry calls target.
    pub pyth_package_id: String,
    pub wormhole_package_id: String,
    pub pyth_state_id: String,
    /// The state's `b"price_info"` table id — pinned because some RPC
    /// providers (publicnode) don't serve the dynamic-field lookup.
    #[serde(default)]
    pub price_info_table_id: Option<String>,
    pub wormhole_state_id: String,

    /// `state::get_base_update_fee` per feed, MIST. 1 on mainnet/testnet.
    #[serde(default = "default_update_fee_mist")]
    pub update_fee_mist: u64,
}

/// Defaults applied to every discovered vault.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VaultDefaults {
    /// Realized-vol window (days) for the options-adapter `VolBook`
    /// premium-mark crank (σ comes from oracle-service).
    pub vol_window_days: u32,
}

impl Default for VaultDefaults {
    fn default() -> Self {
        Self {
            vol_window_days: default_vol_window_days(),
        }
    }
}

impl KeeperConfig {
    pub fn load(path: &Path) -> Result<Self> {
        config_load::load_toml(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped per-env configs must deserialize — they're what the
    /// Dockerfile entrypoint loads on the deployed box. Vault identity
    /// is discovered, so neither carries vault entries; both pin the
    /// verified Sui-testnet Pyth/Wormhole ids and the BETA Hermes (the
    /// feed set testnet PriceInfoObjects are keyed by).
    #[test]
    fn shipped_env_configs_parse() {
        for (env, raw) in [
            ("staging", include_str!("../config/config.staging.toml")),
            ("prod", include_str!("../config/config.prod.toml")),
        ] {
            let cfg: KeeperConfig =
                toml::from_str(raw).unwrap_or_else(|e| panic!("config.{env}.toml: {e}"));
            assert_eq!(cfg.indexer_graphql_url, "http://indexer:9002/graphql", "{env}");
            assert_eq!(cfg.tick_secs, 15, "{env}");
            assert_eq!(cfg.health_addr.port(), 8086, "{env}");
            assert_eq!(
                cfg.pyth.hermes_url, "https://hermes-beta.pyth.network",
                "{env}: testnet PriceInfoObjects are keyed by BETA feed ids"
            );
            assert!(cfg.pyth.pyth_state_id.starts_with("0x243759"), "{env}");
            assert!(cfg.pyth.wormhole_state_id.starts_with("0x31358d"), "{env}");
            assert_eq!(cfg.pyth.update_fee_mist, 1, "{env}");
            assert_eq!(cfg.vault_defaults.vol_window_days, 30, "{env}");
        }
    }

    /// `[external]` defaults apply when the section is absent (as in the
    /// shipped configs), and an explicit section overrides them.
    #[test]
    fn external_section_defaults_and_overrides() {
        let cfg: KeeperConfig =
            toml::from_str(include_str!("../config/config.staging.toml")).unwrap();
        assert_eq!(cfg.external.reconciliation_tolerance_bps, 2_000);
        assert_eq!(cfg.external.equity_stale_alert_ms, 3_600_000);
        assert!(cfg.external.equity_posts.is_empty());

        let cfg: KeeperConfig = toml::from_str(
            r#"
            indexer_graphql_url = "http://x/graphql"
            [pyth]
            pyth_package_id = "0x1"
            wormhole_package_id = "0x1"
            pyth_state_id = "0x1"
            wormhole_state_id = "0x1"
            [external]
            reconciliation_tolerance_bps = 500
            [external.equity_posts]
            "0xabc" = 1000000
            "#,
        )
        .unwrap();
        assert_eq!(cfg.external.reconciliation_tolerance_bps, 500);
        assert_eq!(cfg.external.equity_stale_alert_ms, 3_600_000);
        assert_eq!(cfg.external.equity_posts.get("0xabc"), Some(&1_000_000));
    }

    /// `[external.bluefin]` parses with defaults; absent ⇒ reader off.
    #[test]
    fn external_bluefin_block_parses() {
        let cfg: KeeperConfig = toml::from_str(
            r#"
            indexer_graphql_url = "http://x/graphql"
            [pyth]
            pyth_package_id = "0x1"
            wormhole_package_id = "0x1"
            pyth_state_id = "0x1"
            wormhole_state_id = "0x1"
            [external.bluefin]
            base_url = "https://api.sui-staging.bluefin.io"
            [external.bluefin.accounts."0xabc"]
            account = "0xf0"
            "#,
        )
        .unwrap();
        let b = cfg.external.bluefin.as_ref().unwrap();
        assert_eq!(b.base_url, "https://api.sui-staging.bluefin.io");
        assert_eq!(b.poll_interval_ms, 10_000);
        assert_eq!(b.max_age_ms, 60_000);
        let a = b.accounts.get("0xabc").unwrap();
        assert_eq!(a.account, "0xf0");
        assert_eq!(a.asset_decimals, 6);

        let cfg: KeeperConfig = toml::from_str(
            r#"
            indexer_graphql_url = "http://x/graphql"
            [pyth]
            pyth_package_id = "0x1"
            wormhole_package_id = "0x1"
            pyth_state_id = "0x1"
            wormhole_state_id = "0x1"
            "#,
        )
        .unwrap();
        assert!(cfg.external.bluefin.is_none());
    }

    /// The example config (local dev) must also parse.
    #[test]
    fn example_config_parses() {
        let cfg: KeeperConfig =
            toml::from_str(include_str!("../config/config.example.toml")).unwrap();
        assert_eq!(cfg.vault_defaults.vol_window_days, 30);
    }
}
