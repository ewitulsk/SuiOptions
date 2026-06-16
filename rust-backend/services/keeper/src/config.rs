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
fn default_benchmarks_url() -> String {
    "https://benchmarks.pyth.network".into()
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythKeeperConfig {
    /// Hermes endpoint serving the SAME feed set the network's
    /// PriceInfoObjects are keyed by: `hermes-beta.pyth.network` for Sui
    /// testnet (beta feed ids), `hermes.pyth.network` for mainnet.
    #[serde(default = "default_hermes_url")]
    pub hermes_url: String,

    /// Historical daily closes for the realized-vol estimate (README §9).
    /// Benchmarks serves the *stable* feed set only; discovered beta feed ids
    /// are mapped to their stable equivalent (`pyth_client::benchmark_feed_id`)
    /// before the lookup. `vault_defaults.sigma_fallback` still backstops
    /// unmapped feeds and benchmark outages.
    #[serde(default = "default_benchmarks_url")]
    pub benchmarks_url: String,

    /// Latest (post-upgrade) Pyth package — the address the
    /// `pyth::pyth` entry calls target.
    pub pyth_package_id: String,
    pub wormhole_package_id: String,
    pub pyth_state_id: String,
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
            sigma_fallback: None,
            vol_window_days: default_vol_window_days(),
            slicing: SlicingConfig::default(),
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

    /// The example config (local dev) must also parse.
    #[test]
    fn example_config_parses() {
        let cfg: KeeperConfig =
            toml::from_str(include_str!("../config/config.example.toml")).unwrap();
        assert_eq!(cfg.vault_defaults.target_delta, 0.20);
        assert_eq!(cfg.vault_defaults.slicing.slices, 4);
    }
}
