//! TOML config for the solana-keeper — the port of the Sui keeper's
//! `config.rs` with the Sui Pyth/Wormhole object handles replaced by the
//! (compile-time-constant, config-overridable) receiver bridge id.
//!
//! Vaults are **discovered**, not configured: the tick loop reads the
//! solana-indexer's `vaults` view and resolves each vault's pinned feeds,
//! mints and decimals from its chain account (`src/discovery.rs`). The
//! config carries only endpoints, the Pyth handles, and the strategy
//! defaults applied to every discovered vault.

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

    /// solana-indexer GraphQL endpoint — vault discovery (`vaults`),
    /// open-auction discovery (`auctions`), bucket candidates for strike
    /// selection, and the current bucket's invalidation flag.
    pub indexer_graphql_url: String,

    pub pyth: PythKeeperConfig,

    /// Strategy knobs applied to every discovered vault.
    #[serde(default)]
    pub vault_defaults: VaultDefaults,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythKeeperConfig {
    /// Hermes endpoint serving the SAME feed set the vaults' pinned feed
    /// ids come from: `hermes-beta.pyth.network` for the devnet/beta set,
    /// `hermes.pyth.network` for mainnet/stable. Used only for on-chain
    /// update data (pyth_leg.rs); spot/σ come from solana-oracle-service.
    #[serde(default = "default_hermes_url")]
    pub hermes_url: String,

    /// Override of the Pyth-operated Wormhole verification bridge that
    /// owns the guardian-set accounts (`solana_tx::pyth::
    /// WORMHOLE_RECEIVER_ID` when absent — same address on mainnet +
    /// devnet; the receiver's on-chain Config pins it).
    #[serde(default)]
    pub wormhole_program_id: Option<String>,
}

/// Strategy defaults applied to every discovered vault (the per-vault
/// chain identity — mints, feeds, decimals — is resolved by
/// `discovery.rs`, not configured). Ports the Sui keeper's struct 1:1.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VaultDefaults {
    /// IV ≈ realized σ × this ratio (calibrated: BTC 1.19, ETH 1.08).
    pub iv_ratio: f64,

    /// Strike-selection delta target (launch memo: 0.20).
    pub target_delta: f64,

    /// Per-cadence override of `target_delta` for short-round (hourly)
    /// vaults — those with `round_ms <= SHORT_ROUND_THRESHOLD_MS`. At an
    /// hourly tenor a 0.20-delta call's premium falls below the reserve,
    /// so these vaults target closer to ATM to stay sellable.
    pub short_round_target_delta: Option<f64>,

    /// σ when the realized-vol fetch fails. No fallback ⇒ the vault
    /// skips strike selection that tick.
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

/// Round duration at/below which a vault is "short cadence" (hourly): its
/// tiny option tenor needs a closer-to-ATM strike to clear the reserve,
/// so `short_round_target_delta` (when set) overrides `target_delta`.
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

/// RFQ slice schedule (ports the Sui keeper's README §6 shape). The
/// keeper opens one auction at a time, sized `deployable /
/// remaining_stagger_slots` (capped at `slices` slots and the vault's
/// `max_slice_amount`); unsold collateral returns to `deployable` and is
/// re-offered while the window is open.
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
    /// deployed container loads. Vault identity is discovered, so none
    /// carries vault entries; all pin the BETA Hermes (devnet vaults pin
    /// beta feed ids).
    #[test]
    fn shipped_env_configs_parse() {
        for (env, raw) in [
            ("dev", include_str!("../config/config.toml")),
            ("staging", include_str!("../config/config.staging.toml")),
            ("prod", include_str!("../config/config.prod.toml")),
        ] {
            let cfg: KeeperConfig =
                toml::from_str(raw).unwrap_or_else(|e| panic!("config.{env}.toml: {e}"));
            assert_eq!(cfg.tick_secs, 15, "{env}");
            assert_eq!(cfg.health_addr.port(), 8086, "{env}");
            assert_eq!(
                cfg.pyth.hermes_url, "https://hermes-beta.pyth.network",
                "{env}: devnet vaults pin BETA feed ids"
            );
            if env == "dev" {
                assert_eq!(cfg.indexer_graphql_url, "http://127.0.0.1:9002/graphql");
            } else {
                assert_eq!(
                    cfg.indexer_graphql_url, "http://solana-indexer:9002/graphql",
                    "{env}"
                );
            }
            // Launch-memo strategy defaults (guide doc 08).
            assert_eq!(cfg.vault_defaults.target_delta, 0.20, "{env}");
            assert_eq!(cfg.vault_defaults.short_round_target_delta, Some(0.25), "{env}");
            assert_eq!(cfg.vault_defaults.iv_ratio, 1.15, "{env}");
            assert_eq!(cfg.vault_defaults.sigma_fallback, Some(0.85), "{env}");
            assert_eq!(cfg.vault_defaults.vol_window_days, 30, "{env}");
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
        assert_eq!(d.target_delta_for(3_600_000), 0.25); // 1h
        assert_eq!(d.target_delta_for(SHORT_ROUND_THRESHOLD_MS), 0.25); // boundary
        assert_eq!(d.target_delta_for(604_800_000), 0.20); // weekly
        let d = VaultDefaults { short_round_target_delta: None, ..d };
        assert_eq!(d.target_delta_for(3_600_000), 0.20);
    }
}
