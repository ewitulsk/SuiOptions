//! TOML config for the option-scheduler.
//!
//! Shape:
//!
//! ```toml
//! indexer_url       = "ws://127.0.0.1:9001/"
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
//!   source = "static"
//!   usd    = 50_000.0
//! ```

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// WS endpoint of the indexer fanout.
    pub indexer_url: String,

    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,

    /// Roll a new family for a pair if `latest_expiry_ms - now_ms` falls
    /// below this. Default = 1 week.
    #[serde(default = "default_roll_threshold_ms")]
    pub roll_threshold_ms: u64,

    /// Configured pairs to roll. Bot is a no-op for any pair not listed.
    pub pairs: Vec<PairConfig>,
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
    /// MVP: hard-code the spot price. `usd` is expressed in conventional
    /// dollars (e.g. 50_000.0 for $50k BTC, 0.15 for DEEP).
    Static { usd: f64 },
    // Future: `Http { url: String, json_path: String }` etc.
}

impl SchedulerConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::from(path).required(true))
            .build()
            .with_context(|| format!("loading {}", path.display()))?;
        settings
            .try_deserialize::<Self>()
            .with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_shape() {
        let toml = r#"
indexer_url       = "ws://127.0.0.1:9001/"
tick_secs         = 60
roll_threshold_ms = 604800000

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
"#;
        let cfg: SchedulerConfig = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        assert_eq!(cfg.pairs.len(), 1);
        assert_eq!(cfg.tick_secs, 60);
        match cfg.pairs[0].spot {
            SpotConfig::Static { usd } => assert_eq!(usd, 50_000.0),
        }
    }
}
