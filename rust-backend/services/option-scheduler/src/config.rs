//! TOML config for the option-scheduler.
//!
//! Shape:
//!
//! ```toml
//! indexer_graphql_url       = "http://127.0.0.1:9002/graphql"
//! tick_secs         = 60
//! roll_threshold_ms = 604_800_000
//!
//! [pyth]
//! hermes_url = "https://hermes.pyth.network"
//!
//! [[pairs]]
//! underlying          = "TBTC"
//! settlement          = "TUSDC"
//! expiry_interval_ms  = 604_800_000
//! strikes_below       = 4
//! strikes_above       = 4
//! interval_pct        = 5.0
//!
//!   [pairs.spot]
//!   source             = "pyth"
//!   max_publish_lag_ms = 30_000
//!   max_conf_bps       = 100
//! ```
//!
//! `[pairs.spot] source = "static"` is still supported for tests and
//! disconnected runs:
//!
//! ```toml
//!   [pairs.spot]
//!   source = "static"
//!   usd    = 50_000.0
//! ```
//!
//! Two cadences for the SAME pair (e.g. a weekly TSUI vault plus an hourly
//! one) coexist by listing the pair twice with a per-pair `roll_threshold_ms`
//! and `[pairs.vault_template]` override. The vault's `round_ms` is the
//! discriminator that keeps the two families/vaults distinct end to end:
//!
//! ```toml
//! [[pairs]]                       # hourly TSUI
//! underlying          = "TSUI"
//! settlement          = "TUSDC"
//! expiry_interval_ms  = 3_600_000 # 1h
//! roll_threshold_ms   = 1_800_000 # roll the next bucket ~30m before expiry
//!   [pairs.spot]
//!   source = "pyth"
//!   [pairs.grid]
//!   mode = "z_ladder"
//!   [pairs.vault_template]        # Path A hourly policy (no contract change)
//!   round_ms           = 3_600_000
//!   selling_window_ms  = 1_800_000
//!   min_expiry_lead_ms = 3_000_000
//!   max_expiry_lead_ms = 4_500_000
//!   rfq_duration_ms    = 300_000  # 5m floor (rfq::MIN_DURATION_MS)
//!   rfq_max_extension_ms = 120_000
//! ```

use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

use crate::roller::ProductType;

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:8083".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// HTTP health-check bind address. Defaults to `0.0.0.0:8083`.
    #[serde(default = "default_health_addr")]
    pub health_addr: std::net::SocketAddr,

    /// Indexer GraphQL query endpoint. Used for just-in-time roll
    /// confirmation and the reconciliation high-water sequence.
    /// e.g. `http://127.0.0.1:9002/graphql`.
    pub indexer_graphql_url: String,

    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    /// Roll a new family for a pair if `latest_expiry_ms - now_ms` falls
    /// below this. Default = 1 week.
    #[serde(default = "default_roll_threshold_ms")]
    pub roll_threshold_ms: u64,

    /// Configured pairs to roll. Bot is a no-op for any pair not listed.
    pub pairs: Vec<PairConfig>,

    /// Postgres URL for the scheduler's rolls DB. Mandatory: the DB is the
    /// single source of truth for which (pair, expiry) slots have been
    /// rolled, and its partial UNIQUE index is the hard guarantee against
    /// duplicate on-chain bucket creation. The scheduler connects at boot
    /// and fails fast if it is unreachable — it never falls back to
    /// indexer-derived state for the roll decision. Supports `${VAR}`
    /// expansion (e.g. `${DB_PASSWORD}`, `${DB_HOST}`) at load time.
    pub scheduler_database_url: String,

    /// Safety margin (in indexer sequences) the reconciler requires
    /// before concluding an ambiguous submit never landed. Default 100.
    #[serde(default = "default_reconciler_safety_margin")]
    pub reconciler_safety_margin: u64,

    /// How often the reconciler task wakes up, in seconds. Default 30.
    #[serde(default = "default_reconciler_interval_secs")]
    pub reconciler_interval_secs: u64,

    /// How long an ambiguous submit may sit unresolved before the reconciler
    /// falls back to the indexer's checkpoint cursor instead of its sequence
    /// margin, in milliseconds. Default 30 minutes.
    ///
    /// `reconciler_safety_margin` counts *events*, so on a quiet chain it can
    /// take weeks to clear — which is how a failed roll stranded staging for
    /// nine days (SO-344). Past this grace period the reconciler asks the
    /// indexer whether it has caught up to the chain tip instead, which
    /// terminates whether or not events are flowing.
    #[serde(default = "default_reconciler_grace_ms")]
    pub reconciler_grace_ms: u64,

    /// How often the vault-ensure pass runs, in milliseconds. Default 1 hour.
    /// Each pass checks every configured pair and creates a vault for any that
    /// lacks one (SO vault auto-provisioning).
    #[serde(default = "default_vault_check_interval_ms")]
    pub vault_check_interval_ms: u64,

    /// Vault policy template. Present ⇒ the scheduler auto-creates one vault
    /// per configured pair (publishing a fresh share coin per vault). Absent ⇒
    /// the vault pass is disabled entirely. Per-asset oracle pins (feed ids,
    /// decimals) come from token-info; everything else comes from here.
    #[serde(default)]
    pub vault_template: Option<VaultTemplate>,
}

fn default_tick_secs() -> u64 {
    60
}

fn default_roll_threshold_ms() -> u64 {
    7 * 24 * 60 * 60 * 1_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairConfig {
    /// Symbol from `deployments.testTokens.tokens`.
    pub underlying: String,
    /// Symbol from `deployments.testTokens.tokens`.
    pub settlement: String,

    /// Cadence between consecutive expiries the scheduler will create.
    pub expiry_interval_ms: u64,

    /// Strikes on either side of spot (e.g. 4 below + 4 above = 9 strikes
    /// including spot, but spot itself doesn't have to be a strike — we
    /// snap to the grid below it).
    pub strikes_below: u32,
    pub strikes_above: u32,

    /// Spacing between adjacent strikes, in percent of spot.
    pub interval_pct: f64,

    /// Covered call (default) vs cash-secured put. Selects the option-coin
    /// codegen and the on-chain `create_bucket` vs `create_put_bucket` entry
    /// for this pair's rolls. Absent ⇒ `call` (back-compat with pre-puts
    /// configs).
    #[serde(default)]
    pub product_type: ProductType,

    pub spot: SpotConfig,

    /// Grid v2 (doc 05 §4.2): when present, strikes come from the
    /// vol-aware z-ladder and `strikes_below`/`strikes_above`/
    /// `interval_pct` are ignored. Absent ⇒ the legacy percentage grid
    /// (kept for test tokens).
    #[serde(default)]
    pub grid: Option<GridConfig>,

    /// Per-pair override of the global `roll_threshold_ms`. Lets a fast
    /// cadence (e.g. an hourly TSUI family) roll on a tight threshold while
    /// weekly families on the same instance keep the week-long default.
    /// Absent ⇒ the global `roll_threshold_ms`.
    #[serde(default)]
    pub roll_threshold_ms: Option<u64>,

    /// Per-pair override of the global `[vault_template]`. Lets the same
    /// scheduler provision an hourly-round vault for this pair while other
    /// pairs use the weekly global template. Absent ⇒ the global template
    /// (and the vault pass is disabled for the pair only if both are absent).
    #[serde(default)]
    pub vault_template: Option<VaultTemplate>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GridConfig {
    /// Strikes at K_i = round_nice(S · exp(z_i · σ · √τ)) with σ =
    /// realized vol clamped to [vol_floor, vol_ceiling].
    ZLadder {
        /// Standard-deviation multiples, ascending. Defaults to the
        /// 5-point ladder (z = 1.30 ≈ the vault's 0.1-delta target);
        /// BTC-style pairs configure the 7-point ladder explicitly.
        #[serde(default = "default_ladder")]
        ladder: Vec<f64>,
        /// Trailing window for realized vol, in days.
        #[serde(default = "default_vol_window_days")]
        vol_window_days: u32,
        #[serde(default = "default_vol_floor")]
        vol_floor: f64,
        #[serde(default = "default_vol_ceiling")]
        vol_ceiling: f64,
        /// Annualized σ used when benchmark history is unavailable —
        /// required for static-spot (test-token) pairs, a safety net for
        /// Pyth pairs. Without it, a failed vol fetch skips the roll.
        #[serde(default)]
        sigma_fallback: Option<f64>,
    },
}

fn default_ladder() -> Vec<f64> {
    pricing::grid::SUI_LADDER.to_vec()
}

fn default_vol_window_days() -> u32 {
    30
}

fn default_vol_floor() -> f64 {
    0.2
}

fn default_vol_ceiling() -> f64 {
    3.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum SpotConfig {
    /// Hard-coded spot. `usd` is in conventional dollars (e.g. 50_000.0
    /// for $50k BTC). Kept for tests and disconnected runs.
    Static { usd: f64 },
    /// Live cross-price via Pyth Hermes. Both legs (underlying and
    /// settlement) are read from `deployments.json::token_info.<sym>.
    /// pythFeedId`; missing feed ids on either side fail the scheduler at
    /// boot. The roll path declines on any guard hit (stale publish, wide
    /// confidence).
    Pyth {
        /// Reject a roll if either feed's `publish_time` is older than
        /// this many milliseconds.
        #[serde(default = "default_max_publish_lag_ms")]
        max_publish_lag_ms: u64,
        /// Reject a roll if `conf / price > max_conf_bps / 10_000` on
        /// either leg. Bps so the threshold reads naturally next to fee
        /// configs elsewhere.
        #[serde(default = "default_max_conf_bps")]
        max_conf_bps: u32,
    },
    // Future: `Http { url: String, json_path: String }` for an arbitrary
    // JSON oracle.
}

/// Per-vault policy applied to every pair the scheduler provisions a vault
/// for. Mirrors `vault::new_config` minus the per-asset oracle pins, which the
/// scheduler fills from token-info (feed ids) and the pair's decimals. Every
/// field has a default (seeded from the contract's reference config) so a bare
/// `[vault_template]` table is enough to switch vault creation on.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VaultTemplate {
    pub mgmt_fee_bps_annual: u64,
    pub perf_fee_bps: u64,
    pub round_ms: u64,
    pub selling_window_ms: u64,
    pub min_strike_bps_over_spot: u64,
    pub max_strike_bps_over_spot: u64,
    pub min_expiry_lead_ms: u64,
    pub max_expiry_lead_ms: u64,
    pub min_reserve_premium_bps: u64,
    pub max_slice_amount: u64,
    pub max_open_rfqs: u64,
    pub rfq_duration_ms: u64,
    pub rfq_snipe_window_ms: u64,
    pub rfq_snipe_extension_ms: u64,
    pub rfq_max_extension_ms: u64,
    pub rfq_min_increment_bps: u64,
    pub hold_premium_in_settlement: bool,
    pub max_swap_slippage_bps: u64,
    pub max_price_age_secs: u64,
    pub max_conf_bps: u64,
}

impl Default for VaultTemplate {
    fn default() -> Self {
        // Reference config from contracts/tests/vault_tests.move::default_config.
        const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
        Self {
            mgmt_fee_bps_annual: 200,                // 2%/yr
            perf_fee_bps: 1_000,                     // 10%
            round_ms: 7 * DAY_MS,                    // weekly
            selling_window_ms: 12 * 60 * 60 * 1_000, // 12h
            min_strike_bps_over_spot: 300,           // ≥ 1.03× spot
            max_strike_bps_over_spot: 6_000,         // ≤ 1.60× spot
            min_expiry_lead_ms: 3 * DAY_MS,
            max_expiry_lead_ms: 9 * DAY_MS,
            min_reserve_premium_bps: 10,
            max_slice_amount: 1_000_000_000_000,
            max_open_rfqs: 4,
            rfq_duration_ms: 400_000,
            rfq_snipe_window_ms: 60_000,
            rfq_snipe_extension_ms: 120_000,
            rfq_max_extension_ms: 100_000,
            rfq_min_increment_bps: 500, // 5%
            hold_premium_in_settlement: false,
            max_swap_slippage_bps: 50,
            max_price_age_secs: 60,
            max_conf_bps: 100,
        }
    }
}

fn default_vault_check_interval_ms() -> u64 {
    60 * 60 * 1_000 // 1 hour
}

fn default_reconciler_safety_margin() -> u64 {
    100
}

fn default_reconciler_interval_secs() -> u64 {
    30
}

fn default_reconciler_grace_ms() -> u64 {
    30 * 60 * 1_000 // 30 minutes
}

fn default_max_publish_lag_ms() -> u64 {
    30_000
}

fn default_max_conf_bps() -> u32 {
    100 // 1%
}

impl SchedulerConfig {
    /// Load the TOML config, expanding `${VAR}` references (e.g.
    /// `${DB_PASSWORD}` in `scheduler_database_url`) against the process
    /// env. A missing referenced env var is a hard error at boot.
    pub fn load(path: &Path) -> Result<Self> {
        config_load::load_toml(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> SchedulerConfig {
        config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    #[test]
    fn database_url_is_mandatory() {
        // The DB is the single source of truth for dedup, so a config
        // without it must fail to parse rather than silently disable the
        // guard.
        let res: Result<SchedulerConfig, _> = config::Config::builder()
            .add_source(config::File::from_str(
                r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 2
strikes_above       = 2
interval_pct        = 5.0

  [pairs.spot]
  source = "static"
  usd    = 50000.0
"#,
                config::FileFormat::Toml,
            ))
            .build()
            .unwrap()
            .try_deserialize();
        assert!(res.is_err(), "missing scheduler_database_url must error");
    }

    #[test]
    fn parses_static_pair() {
        let cfg = parse(
            r#"
indexer_graphql_url       = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "static"
  usd    = 50000.0
"#,
        );
        assert_eq!(cfg.pairs.len(), 1);
        assert_eq!(
            cfg.scheduler_database_url,
            "postgresql://postgres:postgres@localhost:5432/scheduler_test"
        );
        assert_eq!(cfg.tick_secs, 60); // default
        match &cfg.pairs[0].spot {
            SpotConfig::Static { usd } => assert_eq!(*usd, 50_000.0),
            other => panic!("expected static, got {other:?}"),
        }
    }

    #[test]
    fn parses_pyth_pair_with_defaults() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "pyth"
"#,
        );
        match &cfg.pairs[0].spot {
            SpotConfig::Pyth {
                max_publish_lag_ms,
                max_conf_bps,
            } => {
                assert_eq!(*max_publish_lag_ms, 30_000);
                assert_eq!(*max_conf_bps, 100);
            }
            other => panic!("expected pyth, got {other:?}"),
        }
    }

    #[test]
    fn pair_without_grid_table_uses_legacy_percent_path() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "static"
  usd    = 50000.0
"#,
        );
        assert!(cfg.pairs[0].grid.is_none());
    }

    /// The shipped prod config must parse, and the hourly TSUI pair must carry
    /// the vol-aware z-ladder grid that keeps its ~1h options sellable.
    #[test]
    fn shipped_prod_config_parses_with_hourly_overrides() {
        let cfg = parse(include_str!("../config/config.prod.toml"));
        let hourly = cfg
            .pairs
            .iter()
            .find(|p| p.underlying == "TSUI" && p.expiry_interval_ms == 3_600_000)
            .expect("hourly TSUI pair");
        match hourly.grid.as_ref().expect("hourly grid configured") {
            GridConfig::ZLadder { ladder, vol_floor, .. } => {
                assert_eq!(ladder.len(), 6, "finer ladder than the default 5-point");
                assert_eq!(*vol_floor, 0.5);
            }
        }
    }

    /// SO-332: the covered-call vault product is deprecated, so none of the
    /// shipped configs may carry a vault template — global or per-pair.
    /// A template anywhere switches the vault-ensure pass back on.
    #[test]
    fn shipped_configs_provision_no_vaults() {
        for (name, src) in [
            ("config.toml", include_str!("../config/config.toml")),
            ("config.staging.toml", include_str!("../config/config.staging.toml")),
            ("config.prod.toml", include_str!("../config/config.prod.toml")),
        ] {
            let cfg = parse(src);
            assert!(cfg.vault_template.is_none(), "{name}: global [vault_template]");
            for p in &cfg.pairs {
                assert!(
                    p.vault_template.is_none(),
                    "{name}: {}/{} carries [pairs.vault_template]",
                    p.underlying,
                    p.settlement
                );
            }
        }
    }

    #[test]
    fn parses_z_ladder_grid_with_defaults() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "SUI"
settlement          = "USDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "pyth"

  [pairs.grid]
  mode = "z_ladder"
"#,
        );
        match cfg.pairs[0].grid.as_ref().unwrap() {
            GridConfig::ZLadder {
                ladder,
                vol_window_days,
                vol_floor,
                vol_ceiling,
                sigma_fallback,
            } => {
                assert_eq!(ladder, &pricing::grid::SUI_LADDER.to_vec());
                assert_eq!(*vol_window_days, 30);
                assert_eq!(*vol_floor, 0.2);
                assert_eq!(*vol_ceiling, 3.0);
                assert!(sigma_fallback.is_none());
            }
        }
    }

    #[test]
    fn parses_z_ladder_grid_with_btc_ladder_and_fallback() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "WBTC"
settlement          = "USDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "static"
  usd    = 117000.0

  [pairs.grid]
  mode           = "z_ladder"
  ladder         = [-0.65, 0.0, 0.65, 1.3, 1.95, 2.6, 3.25]
  sigma_fallback = 0.55
"#,
        );
        match cfg.pairs[0].grid.as_ref().unwrap() {
            GridConfig::ZLadder { ladder, sigma_fallback, .. } => {
                assert_eq!(ladder.len(), 7);
                assert_eq!(*sigma_fallback, Some(0.55));
            }
        }
    }

    #[test]
    fn parses_per_pair_roll_threshold_and_vault_template_override() {
        // Coexistence: an hourly TSUI family on the same instance as weekly
        // pairs carries its own roll threshold and vault policy.
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"
roll_threshold_ms = 604800000

[vault_template]
round_ms = 604800000

[[pairs]]
underlying          = "TSUI"
settlement          = "TUSDC"
expiry_interval_ms  = 3600000
strikes_below       = 2
strikes_above       = 2
interval_pct        = 5.0
roll_threshold_ms   = 1800000

  [pairs.spot]
  source = "pyth"

  [pairs.vault_template]
  round_ms           = 3600000
  selling_window_ms  = 1800000
  min_expiry_lead_ms = 3000000
  max_expiry_lead_ms = 4500000
"#,
        );
        let pair = &cfg.pairs[0];
        assert_eq!(pair.roll_threshold_ms, Some(1_800_000));
        let t = pair.vault_template.as_ref().expect("per-pair template");
        assert_eq!(t.round_ms, 3_600_000);
        assert_eq!(t.selling_window_ms, 1_800_000);
        assert_eq!(t.min_expiry_lead_ms, 3_000_000);
        assert_eq!(t.max_expiry_lead_ms, 4_500_000);
        // Unspecified fields fall back to the VaultTemplate struct defaults.
        assert_eq!(t.mgmt_fee_bps_annual, 200);
        // Global template still parses independently (weekly default).
        assert_eq!(cfg.vault_template.as_ref().unwrap().round_ms, 604_800_000);
    }

    #[test]
    fn pair_without_overrides_leaves_them_none() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "static"
  usd    = 50000.0
"#,
        );
        assert_eq!(cfg.pairs[0].roll_threshold_ms, None);
        assert!(cfg.pairs[0].vault_template.is_none());
    }

    #[test]
    fn parses_pyth_pair_with_explicit_guards() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source             = "pyth"
  max_publish_lag_ms = 10000
  max_conf_bps       = 50
"#,
        );
        match &cfg.pairs[0].spot {
            SpotConfig::Pyth {
                max_publish_lag_ms,
                max_conf_bps,
            } => {
                assert_eq!(*max_publish_lag_ms, 10_000);
                assert_eq!(*max_conf_bps, 50);
            }
            other => panic!("expected pyth, got {other:?}"),
        }
    }
}
