//! TOML config for the option-scheduler.
//!
//! Shape:
//!
//! ```toml
//! indexer_url       = "ws://127.0.0.1:9001/"
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

use std::path::Path;

use anyhow::Result;
use runtime_config::config_load;
use serde::Deserialize;

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:8083".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// HTTP health-check bind address. Defaults to `0.0.0.0:8083`.
    #[serde(default = "default_health_addr")]
    pub health_addr: std::net::SocketAddr,

    /// WS endpoint of the indexer fanout.
    pub indexer_url: String,

    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    /// Roll a new family for a pair if `latest_expiry_ms - now_ms` falls
    /// below this. Default = 1 week.
    #[serde(default = "default_roll_threshold_ms")]
    pub roll_threshold_ms: u64,

    /// Pyth Hermes endpoint settings. Only consulted when at least one
    /// pair uses `source = "pyth"`, but loaded eagerly so a typo surfaces
    /// at startup.
    #[serde(default)]
    pub pyth: PythGlobalConfig,

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
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythGlobalConfig {
    /// Base URL for the Pyth Hermes REST API. The scheduler issues exactly
    /// one `GET /v2/updates/price/latest?ids[]=…&ids[]=…` per pair per
    /// roll, so the public endpoint's rate cap is plenty.
    #[serde(default = "default_hermes_url")]
    pub hermes_url: String,
}

impl Default for PythGlobalConfig {
    fn default() -> Self {
        Self {
            hermes_url: default_hermes_url(),
        }
    }
}

fn default_hermes_url() -> String {
    "https://hermes.pyth.network".into()
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

    pub spot: SpotConfig,
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
indexer_url = "ws://127.0.0.1:9001/"

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
indexer_url       = "ws://127.0.0.1:9001/"
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
        assert_eq!(cfg.pyth.hermes_url, "https://hermes.pyth.network");
        match &cfg.pairs[0].spot {
            SpotConfig::Static { usd } => assert_eq!(*usd, 50_000.0),
            other => panic!("expected static, got {other:?}"),
        }
    }

    #[test]
    fn parses_pyth_pair_with_defaults() {
        let cfg = parse(
            r#"
indexer_url = "ws://127.0.0.1:9001/"
scheduler_database_url = "postgresql://postgres:postgres@localhost:5432/scheduler_test"

[pyth]
hermes_url = "https://hermes.custom/"

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
        assert_eq!(cfg.pyth.hermes_url, "https://hermes.custom/");
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
    fn parses_pyth_pair_with_explicit_guards() {
        let cfg = parse(
            r#"
indexer_url = "ws://127.0.0.1:9001/"
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
