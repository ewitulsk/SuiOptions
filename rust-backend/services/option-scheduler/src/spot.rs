//! Spot price source.
//!
//! MVP only ships the `static` source — the value is hard-coded in
//! `config.toml`. The struct hides this behind a trait-ish enum so a future
//! `http` source (Binance / Coingecko) can be added without touching the
//! tick loop.

use anyhow::Result;

use crate::config::SpotConfig;

#[derive(Debug, Clone)]
pub enum SpotSource {
    Static { usd: f64 },
}

impl SpotSource {
    pub fn from_config(cfg: &SpotConfig) -> Self {
        match *cfg {
            SpotConfig::Static { usd } => SpotSource::Static { usd },
        }
    }

    /// Current spot price in conventional USD per underlying unit
    /// (e.g. 50_000.0 for BTC, 0.15 for DEEP). The strike-grid module
    /// converts this into chain-units.
    pub async fn fetch_usd(&self) -> Result<f64> {
        match self {
            SpotSource::Static { usd } => Ok(*usd),
        }
    }
}
