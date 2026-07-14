//! TOML config for the solana-option-scheduler. Mirrors the Sui twin's
//! shape; `scheduler_database_url` points at `solana_scheduler_<env>` and
//! the default ops port is 8087 (the Solana scheduler's slot).
//!
//! ```toml
//! indexer_graphql_url    = "http://127.0.0.1:9002/graphql"
//! scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/solana_scheduler"
//! tick_secs         = 60
//! roll_threshold_ms = 604_800_000
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
//! `[pairs.spot] source = "static"` is supported for tests and disconnected
//! runs; `[pairs.grid] mode = "z_ladder"` switches the pair to the vol-aware
//! ladder. Two cadences for the SAME pair coexist by listing the pair twice
//! with per-pair `roll_threshold_ms` / `[pairs.vault_template]` overrides —
//! the vault's `round_ms` is the discriminator end to end.

use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

use crate::roller::ProductType;

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:8087".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// HTTP health/metrics bind address. Defaults to `0.0.0.0:8087`.
    #[serde(default = "default_health_addr")]
    pub health_addr: std::net::SocketAddr,

    /// solana-indexer GraphQL query endpoint. Used for just-in-time roll
    /// confirmation and the reconciliation high-water sequence — never for
    /// the roll decision. e.g. `http://127.0.0.1:9002/graphql`.
    pub indexer_graphql_url: String,

    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    /// Roll a new family for a pair if `latest_expiry_ms - now_ms` falls
    /// below this. Default = 1 week.
    #[serde(default = "default_roll_threshold_ms")]
    pub roll_threshold_ms: u64,

    /// Configured pairs to roll. Bot is a no-op for any pair not listed.
    pub pairs: Vec<PairConfig>,

    /// Postgres URL for the scheduler's rolls DB (`solana_scheduler_<env>`).
    /// Mandatory: the DB is the single source of truth for which
    /// (pair, expiry) slots have been rolled, and its partial UNIQUE index is
    /// the hard guarantee against duplicate on-chain bucket creation. The
    /// scheduler connects at boot and fails fast if it is unreachable — it
    /// never falls back to indexer-derived state for the roll decision.
    /// Supports `${VAR}` expansion (e.g. `${DB_PASSWORD}`, `${DB_HOST}`).
    pub scheduler_database_url: String,

    /// Safety margin (in indexer sequences) the reconciler requires before
    /// concluding an ambiguous submit never landed. Default 100.
    #[serde(default = "default_reconciler_safety_margin")]
    pub reconciler_safety_margin: u64,

    /// How often the reconciler task wakes up, in seconds. Default 30.
    #[serde(default = "default_reconciler_interval_secs")]
    pub reconciler_interval_secs: u64,

    /// How often the vault-ensure pass runs, in milliseconds. Default 1 hour.
    #[serde(default = "default_vault_check_interval_ms")]
    pub vault_check_interval_ms: u64,

    /// Vault policy template. Present ⇒ the scheduler auto-creates one vault
    /// per configured call pair with Pyth feeds on both legs. Absent ⇒ the
    /// vault pass is disabled entirely. Per-asset oracle pins (feed ids,
    /// decimals) come from solana-token-info; everything else from here.
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
    /// Ticker from the solana-token-info catalog.
    pub underlying: String,
    /// Ticker from the solana-token-info catalog.
    pub settlement: String,

    /// Cadence between consecutive expiries the scheduler will create.
    pub expiry_interval_ms: u64,

    /// Strikes on either side of spot for the legacy percentage grid.
    pub strikes_below: u32,
    pub strikes_above: u32,

    /// Spacing between adjacent strikes, in percent of spot.
    pub interval_pct: f64,

    /// Covered call (default) vs cash-secured put. Selects `create_bucket`
    /// vs `create_put_bucket` for this pair's rolls.
    #[serde(default)]
    pub product_type: ProductType,

    pub spot: SpotConfig,

    /// Grid v2: when present, strikes come from the vol-aware z-ladder and
    /// `strikes_below`/`strikes_above`/`interval_pct` are ignored. Absent ⇒
    /// the legacy percentage grid (kept for test tokens).
    #[serde(default)]
    pub grid: Option<GridConfig>,

    /// Per-pair override of the global `roll_threshold_ms`.
    #[serde(default)]
    pub roll_threshold_ms: Option<u64>,

    /// Per-pair override of the global `[vault_template]`.
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
    /// Hard-coded spot. `usd` is in conventional dollars. Kept for tests
    /// and disconnected runs.
    Static { usd: f64 },
    /// Live cross-price via solana-oracle-service. Both legs read their
    /// `pyth_feed_id` from the solana-token-info catalog; missing feed ids
    /// on either side fail the scheduler at boot.
    Pyth {
        /// Reject a roll if either feed's `publish_time` is older than
        /// this many milliseconds.
        #[serde(default = "default_max_publish_lag_ms")]
        max_publish_lag_ms: u64,
        /// Reject a roll if `conf / price > max_conf_bps / 10_000` on
        /// either leg.
        #[serde(default = "default_max_conf_bps")]
        max_conf_bps: u32,
    },
}

/// Per-vault policy applied to every pair the scheduler provisions a vault
/// for. Maps 1:1 to the on-chain `options_vault::state::VaultConfig` minus
/// the per-asset oracle pins, which the scheduler fills from the
/// solana-token-info catalog (feed ids) and the pair's decimals. Every field
/// has a default (seeded from the contract's reference config) so a bare
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
        // Reference config carried over from the Sui twin (the contract
        // tests' default_config); passes options_vault::state::validate_config.
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
    fn parses_static_pair_with_defaults() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/solana_scheduler_test"

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
        assert_eq!(cfg.tick_secs, 60); // default
        assert_eq!(cfg.roll_threshold_ms, 604_800_000); // default
        assert_eq!(cfg.reconciler_safety_margin, 100); // default
        assert_eq!(cfg.reconciler_interval_secs, 30); // default
        assert_eq!(cfg.health_addr.port(), 8087); // the Solana scheduler slot
        assert_eq!(cfg.pairs[0].product_type, ProductType::Call); // default
        match &cfg.pairs[0].spot {
            SpotConfig::Static { usd } => assert_eq!(*usd, 50_000.0),
            other => panic!("expected static, got {other:?}"),
        }
        assert!(cfg.pairs[0].grid.is_none());
        assert!(cfg.vault_template.is_none());
    }

    #[test]
    fn parses_pyth_pair_with_guard_defaults_and_overrides() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://x"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "pyth"

[[pairs]]
underlying          = "TSOL"
settlement          = "TUSDC"
product_type        = "put"
expiry_interval_ms  = 604800000
strikes_below       = 2
strikes_above       = 2
interval_pct        = 10.0

  [pairs.spot]
  source             = "pyth"
  max_publish_lag_ms = 10000
  max_conf_bps       = 50
"#,
        );
        match &cfg.pairs[0].spot {
            SpotConfig::Pyth { max_publish_lag_ms, max_conf_bps } => {
                assert_eq!(*max_publish_lag_ms, 30_000);
                assert_eq!(*max_conf_bps, 100);
            }
            other => panic!("expected pyth, got {other:?}"),
        }
        assert_eq!(cfg.pairs[1].product_type, ProductType::Put);
        match &cfg.pairs[1].spot {
            SpotConfig::Pyth { max_publish_lag_ms, max_conf_bps } => {
                assert_eq!(*max_publish_lag_ms, 10_000);
                assert_eq!(*max_conf_bps, 50);
            }
            other => panic!("expected pyth, got {other:?}"),
        }
    }

    #[test]
    fn parses_z_ladder_grid_with_defaults_and_explicit_ladder() {
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://x"

[[pairs]]
underlying          = "TSOL"
settlement          = "TUSDC"
expiry_interval_ms  = 604800000
strikes_below       = 4
strikes_above       = 4
interval_pct        = 5.0

  [pairs.spot]
  source = "pyth"

  [pairs.grid]
  mode = "z_ladder"

[[pairs]]
underlying          = "TBTC"
settlement          = "TUSDC"
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
        match cfg.pairs[1].grid.as_ref().unwrap() {
            GridConfig::ZLadder { ladder, sigma_fallback, .. } => {
                assert_eq!(ladder.len(), 7);
                assert_eq!(*sigma_fallback, Some(0.55));
            }
        }
    }

    #[test]
    fn parses_per_pair_roll_threshold_and_vault_template_override() {
        // Coexistence: an hourly family on the same instance as weekly
        // pairs carries its own roll threshold and vault policy.
        let cfg = parse(
            r#"
indexer_graphql_url = "http://127.0.0.1:9002/graphql"
scheduler_database_url = "postgresql://x"
roll_threshold_ms = 604800000

[vault_template]
round_ms = 604800000

[[pairs]]
underlying          = "TSOL"
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
    fn vault_template_defaults_pass_onchain_validation() {
        // The on-chain create rejects ConfigInvalid; the shipped defaults
        // must clear options_vault's validate_config for any feed pins.
        let t = VaultTemplate::default();
        let spec = crate::vault_roller::VaultPairSpec {
            underlying_symbol: "TSOL".into(),
            settlement_symbol: "TUSDC".into(),
            underlying_mint: "So11111111111111111111111111111111111111112".into(),
            settlement_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            underlying_decimals: 9,
            settlement_decimals: 6,
            underlying_feed_id: [1u8; 32],
            settlement_feed_id: [2u8; 32],
        };
        let config = crate::vault_roller::build_vault_config(&spec, &t);
        assert!(options_vault::state::validate_config(&config));
        assert_eq!(config.underlying_decimals, 9);
        assert_eq!(config.underlying_feed_id, [1u8; 32]);
    }

    /// The shipped configs must parse (env-var refs replaced for the test).
    #[test]
    fn shipped_configs_parse() {
        for raw in [
            include_str!("../config/config.toml"),
            include_str!("../config/config.staging.toml"),
            include_str!("../config/config.prod.toml"),
        ] {
            let sanitized = raw
                .replace("${DB_PASSWORD}", "pw")
                .replace("${DB_HOST}", "localhost");
            let cfg = parse(&sanitized);
            assert!(!cfg.pairs.is_empty());
            // The shipped examples carry the two reference pairs.
            assert!(cfg.pairs.iter().any(|p| p.underlying == "TBTC"));
            assert!(cfg.pairs.iter().any(|p| p.underlying == "TSOL"));
        }
    }
}
