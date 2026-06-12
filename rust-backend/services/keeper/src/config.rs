//! TOML config for the vault-keeper (README §11).
//!
//! ```toml
//! indexer_graphql_url = "http://127.0.0.1:9002/graphql"
//! tick_secs = 15
//!
//! [pyth]
//! hermes_url          = "https://hermes.pyth.network"
//! benchmarks_url      = "https://benchmarks.pyth.network"
//! pyth_package_id     = "0x…"   # latest (upgraded) pyth package
//! wormhole_package_id = "0x…"
//! pyth_state_id       = "0x…"
//! wormhole_state_id   = "0x…"
//!
//! [[vaults]]
//! vault_id   = "0x…"
//! underlying = "SUI"            # token-info tickers (types/decimals/feeds)
//! settlement = "USDC"
//! share_type = "0x…::vshare::VSHARE"
//! underlying_price_info = "0x…" # shared PriceInfoObject ids
//! settlement_price_info = "0x…"
//! iv_ratio = 1.15
//! sigma_fallback = 0.85
//! [vaults.slicing]
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

    /// Indexer GraphQL endpoint — bucket candidates for strike selection
    /// and the call type of the round's current bucket.
    pub indexer_graphql_url: String,

    pub pyth: PythKeeperConfig,

    pub vaults: Vec<VaultEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythKeeperConfig {
    #[serde(default = "default_hermes_url")]
    pub hermes_url: String,

    /// Historical daily closes for the realized-vol estimate (README §9).
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

#[derive(Debug, Clone, Deserialize)]
pub struct VaultEntry {
    pub vault_id: String,

    /// token-info tickers; resolve coin types, decimals, and Pyth feed
    /// ids from the catalog.
    pub underlying: String,
    pub settlement: String,

    /// Fully-qualified VShare coin type (the vault's third generic).
    pub share_type: String,

    /// Shared `PriceInfoObject` ids for the two pinned feeds.
    pub underlying_price_info: String,
    pub settlement_price_info: String,

    /// Keeper-owned `Coin<DEEP>` to fund DeepBook taker fees on
    /// `swap_proceeds`, plus the amount to split per swap. `None` ⇒ a
    /// zero coin (fine on whitelisted pools).
    pub deep_funding_coin: Option<String>,
    pub deep_fee_per_swap: Option<u64>,

    /// IV ≈ realized σ × this ratio (calibrated: BTC 1.19, ETH 1.08).
    #[serde(default = "default_iv_ratio")]
    pub iv_ratio: f64,

    /// σ when the Benchmarks fetch fails. No fallback ⇒ the vault skips
    /// strike selection that tick.
    pub sigma_fallback: Option<f64>,

    #[serde(default = "default_vol_window_days")]
    pub vol_window_days: u32,

    #[serde(default)]
    pub slicing: SlicingConfig,
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
