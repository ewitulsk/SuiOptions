//! TOML config for the vault-keeper (README §11).
//!
//! Vaults are **discovered**, not configured: the tick loop reads the
//! indexer's `vaults` view (fed by `VaultCreated`) and resolves each
//! vault's pinned feeds / decimals from its chain object and its
//! `PriceInfoObject`s from the Pyth state table (`src/discovery.rs`).
//! The config carries only the endpoints, the Pyth deployment handles,
//! and the strategy defaults applied to every discovered vault.
//!
//! ```toml
//! indexer_graphql_url = "http://127.0.0.1:9002/graphql"
//! tick_secs = 15
//!
//! [pyth]
//! hermes_url          = "https://hermes-beta.pyth.network"  # testnet = beta feeds
//! benchmarks_url      = "https://benchmarks.pyth.network"
//! pyth_package_id     = "0x…"   # latest (upgraded) pyth package
//! wormhole_package_id = "0x…"
//! pyth_state_id       = "0x…"
//! wormhole_state_id   = "0x…"
//!
//! [vault_defaults]
//! iv_ratio = 1.15
//! target_delta = 0.20
//! sigma_fallback = 0.85
//! [vault_defaults.slicing]
//! slices = 4
//! stagger_minutes = 90
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
fn default_hermes_url() -> String {
    "https://hermes.pyth.network".into()
}
fn default_update_fee_mist() -> u64 {
    1
}
fn default_iv_ratio() -> f64 {
    1.15
}
fn default_target_delta() -> f64 {
    0.10
}
fn default_vol_window_days() -> u32 {
    30
}
fn default_slices() -> u64 {
    4
}
fn default_stagger_minutes() -> u64 {
    90
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

    /// Indexer GraphQL endpoint — vault discovery (`vaults` view),
    /// bucket candidates for strike selection, and the call type of the
    /// round's current bucket.
    pub indexer_graphql_url: String,

    pub pyth: PythKeeperConfig,

    /// Strategy knobs applied to every discovered vault.
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
    /// (`"0x…"`) → target equity in deposit-asset units. Real venue
    /// readers (Bluefin / DBM) are follow-ups; empty ⇒ posting disabled.
    pub equity_posts: std::collections::BTreeMap<String, u64>,

    /// Trustless DeepBook-Margin equity legs (SO-299 phase C): vault id
    /// (`"0x…"`) → the vault's MarginManager identity. A listed vault's
    /// appraisals compose `dbm_oracle::record{,_no_debt}` instead of the
    /// attested `equity_oracle::record` (the dbm-oracle package id comes
    /// from the token-info snapshot).
    pub dbm: std::collections::BTreeMap<String, DbmVaultConfig>,
}

/// One `[external.dbm."0x…"]` block: the vault's DBM account identity.
#[derive(Debug, Clone, Deserialize)]
pub struct DbmVaultConfig {
    pub margin_manager_id: String,
    pub deepbook_pool_id: String,
    pub base_margin_pool_id: String,
    pub quote_margin_pool_id: String,
    pub base_type: String,
    pub quote_type: String,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            reconciliation_tolerance_bps: default_reconciliation_tolerance_bps(),
            equity_stale_alert_ms: default_equity_stale_alert_ms(),
            equity_posts: Default::default(),
            dbm: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythKeeperConfig {
    /// Hermes endpoint serving the SAME feed set the network's
    /// PriceInfoObjects are keyed by: `hermes-beta.pyth.network` for Sui
    /// testnet (beta feed ids), `hermes.pyth.network` for mainnet. Used only
    /// for the on-chain VAA (submit.rs); spot/σ come from oracle-service.
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

/// Strategy defaults applied to every discovered vault (the per-vault
/// chain identity — types, feeds, decimals, price-info objects — is
/// resolved by `discovery.rs`, not configured).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VaultDefaults {
    /// IV ≈ realized σ × this ratio (calibrated: BTC 1.19, ETH 1.08).
    pub iv_ratio: f64,

    /// Strike-selection delta target. 0.10 is the doc 04 design point;
    /// the SUI launch memo (guide doc 08) picks 0.20 — at 0.10 the grid
    /// snap-up plus the auction haircut leaves too little premium above
    /// the reserve floor.
    pub target_delta: f64,

    /// Per-cadence override of `target_delta` for short-round (hourly)
    /// vaults — those with `round_ms <= SHORT_ROUND_THRESHOLD_MS`. At an
    /// hourly tenor a 0.20-delta call's premium falls below the reserve, so
    /// these vaults target closer to ATM (higher delta) to stay sellable.
    /// Absent ⇒ `target_delta` applies to every cadence.
    pub short_round_target_delta: Option<f64>,

    /// σ when the Benchmarks fetch fails (always, on testnet — beta
    /// feed ids aren't served there). No fallback ⇒ the vault skips
    /// strike selection that tick.
    pub sigma_fallback: Option<f64>,

    pub vol_window_days: u32,

    pub slicing: SlicingConfig,
}

impl Default for VaultDefaults {
    fn default() -> Self {
        Self {
            iv_ratio: default_iv_ratio(),
            target_delta: default_target_delta(),
            short_round_target_delta: None,
            sigma_fallback: None,
            vol_window_days: default_vol_window_days(),
            slicing: SlicingConfig::default(),
        }
    }
}

/// Round duration at/below which a vault is "short cadence" (hourly): its tiny
/// option tenor needs a closer-to-ATM strike to clear the reserve, so
/// `short_round_target_delta` (when set) overrides `target_delta`.
pub const SHORT_ROUND_THRESHOLD_MS: u64 = 6 * 3_600_000; // 6h

impl VaultDefaults {
    /// The strike-selection delta target for a vault of this cadence.
    pub fn target_delta_for(&self, round_ms: u64) -> f64 {
        if round_ms <= SHORT_ROUND_THRESHOLD_MS {
            self.short_round_target_delta.unwrap_or(self.target_delta)
        } else {
            self.target_delta
        }
    }
}

/// RFQ slice schedule (README §6). The keeper opens one auction at a
/// time, sized `deployable / remaining_stagger_slots` (capped at
/// `slices` slots and the vault's `max_slice_amount`); unsold collateral
/// returns to `deployable` and is re-offered while the window is open.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct SlicingConfig {
    #[serde(default = "default_slices")]
    pub slices: u64,
    #[serde(default = "default_stagger_minutes")]
    pub stagger_minutes: u64,
}

impl Default for SlicingConfig {
    fn default() -> Self {
        Self {
            slices: default_slices(),
            stagger_minutes: default_stagger_minutes(),
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
            // Launch-memo strategy defaults (guide doc 08).
            assert_eq!(cfg.vault_defaults.target_delta, 0.20, "{env}");
            // Hourly (short-round) vaults target closer to ATM to clear the reserve.
            assert_eq!(
                cfg.vault_defaults.short_round_target_delta,
                Some(0.25),
                "{env}"
            );
            assert_eq!(cfg.vault_defaults.target_delta_for(3_600_000), 0.25, "{env}: hourly");
            assert_eq!(cfg.vault_defaults.target_delta_for(604_800_000), 0.20, "{env}: weekly");
            assert_eq!(cfg.vault_defaults.iv_ratio, 1.15, "{env}");
            assert_eq!(
                cfg.vault_defaults.sigma_fallback,
                Some(0.85),
                "{env}: sigma_fallback backstops unmapped feeds / benchmark outages"
            );
            assert_eq!(cfg.vault_defaults.slicing.slices, 4, "{env}");
            assert_eq!(cfg.vault_defaults.slicing.stagger_minutes, 90, "{env}");
        }
    }

    #[test]
    fn target_delta_for_overrides_only_short_rounds() {
        let d = VaultDefaults {
            target_delta: 0.20,
            short_round_target_delta: Some(0.25),
            ..VaultDefaults::default()
        };
        // Hourly (≤ 6h) takes the override; weekly keeps the global target.
        assert_eq!(d.target_delta_for(3_600_000), 0.25); // 1h
        assert_eq!(d.target_delta_for(SHORT_ROUND_THRESHOLD_MS), 0.25); // boundary
        assert_eq!(d.target_delta_for(604_800_000), 0.20); // weekly
        // No override ⇒ global target at every cadence.
        let d = VaultDefaults { short_round_target_delta: None, ..d };
        assert_eq!(d.target_delta_for(3_600_000), 0.20);
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
        assert!(cfg.external.dbm.is_empty());
    }

    /// `[external.dbm."0x…"]` blocks parse into per-vault DBM identities.
    #[test]
    fn external_dbm_blocks_parse() {
        let cfg: KeeperConfig = toml::from_str(
            r#"
            indexer_graphql_url = "http://x/graphql"
            [pyth]
            pyth_package_id = "0x1"
            wormhole_package_id = "0x1"
            pyth_state_id = "0x1"
            wormhole_state_id = "0x1"
            [external.dbm."0xabc"]
            margin_manager_id = "0x11"
            deepbook_pool_id = "0x12"
            base_margin_pool_id = "0x13"
            quote_margin_pool_id = "0x14"
            base_type = "0x2::sui::SUI"
            quote_type = "0xa::dbusdc::DBUSDC"
            "#,
        )
        .unwrap();
        let d = cfg.external.dbm.get("0xabc").unwrap();
        assert_eq!(d.margin_manager_id, "0x11");
        assert_eq!(d.deepbook_pool_id, "0x12");
        assert_eq!(d.base_margin_pool_id, "0x13");
        assert_eq!(d.quote_margin_pool_id, "0x14");
        assert_eq!(d.base_type, "0x2::sui::SUI");
        assert_eq!(d.quote_type, "0xa::dbusdc::DBUSDC");
    }

    /// The example config (local dev) must also parse.
    #[test]
    fn example_config_parses() {
        let cfg: KeeperConfig =
            toml::from_str(include_str!("../config/config.example.toml")).unwrap();
        assert_eq!(cfg.vault_defaults.target_delta, 0.20);
        assert_eq!(cfg.vault_defaults.slicing.slices, 4);
    }
}
