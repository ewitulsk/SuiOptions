//! Spot price source. Port of the Sui twin's `spot.rs` — the only change is
//! that token specs come from solana-token-info's catalog.
//!
//! Two variants:
//!
//! - `static` — a hardcoded USD price from `config.toml`. Useful for
//!   tests, offline dry-runs, and pairs whose price genuinely doesn't move.
//! - `pyth` — live USD-cross via oracle-service. Reads the latest cached
//!   price for both legs, validates each one against the configured
//!   staleness / confidence guards, and returns the cross.
//!
//! Both variants return the same thing: the strike-grid "spot" as a USD
//! cross (settlement units per underlying). The strike-grid module never sees
//! USD-vs-chain-unit scaling — it operates on the f64 the resolver hands back.

use anyhow::{Context, Result};
use oracle_client::{OracleClient, PriceFeedId, PricePoint};
use thiserror::Error;
use tracing::info;

use crate::config::SpotConfig;

/// Spot source with all chain-relevant inputs already resolved against the
/// token catalog. Built once at boot per pair so a Pyth misconfig (missing
/// `pyth_feed_id`, malformed hex) fails before the first tick.
#[derive(Debug, Clone)]
pub enum ResolvedSpotSource {
    Static {
        /// USD cross at boot. Doesn't change over the life of the process.
        spot_usd_cross: f64,
    },
    Pyth {
        underlying_feed: PriceFeedId,
        settlement_feed: PriceFeedId,
        /// Reject a roll if either feed's `publish_time` is older than
        /// this. The strike grid only updates weekly, so we can afford to
        /// be strict here without hurting availability.
        max_publish_lag_ms: u64,
        /// Reject a roll if `conf / price` exceeds this on either leg.
        max_conf_bps: u32,
    },
}

/// Structured reasons why a Pyth resolve declined to produce a spot.
#[derive(Debug, Error, PartialEq)]
pub enum SpotError {
    #[error("missing pyth feed for {leg} (feed_id={feed_id})")]
    MissingFeed { leg: &'static str, feed_id: String },

    #[error("non-positive or non-finite {leg} price: {price}")]
    BadPrice { leg: &'static str, price: f64 },

    #[error("{leg} publish_time is stale: {age_ms}ms old, max {max_ms}ms")]
    StalePrice {
        leg: &'static str,
        age_ms: i64,
        max_ms: u64,
    },

    #[error("{leg} confidence is too wide: {conf_bps:.2} bps, max {max_bps} bps")]
    WideConfidence {
        leg: &'static str,
        conf_bps: f64,
        max_bps: u32,
    },

    #[error("cross is out of range or non-finite: {scaled}")]
    OutOfRange { scaled: f64 },
}

impl ResolvedSpotSource {
    /// Build a resolved source from the per-pair config + the two token
    /// catalog entries. The Pyth path requires both legs to carry a
    /// `pyth_feed_id`; missing either one fails at boot.
    pub fn from_config(
        cfg: &SpotConfig,
        underlying_spec: &solana_token_info_client::SupportedToken,
        settlement_spec: &solana_token_info_client::SupportedToken,
    ) -> Result<Self> {
        match *cfg {
            SpotConfig::Static { usd } => {
                if !usd.is_finite() || usd <= 0.0 {
                    return Err(anyhow::anyhow!(
                        "static spot must be positive and finite: {usd}"
                    ));
                }
                Ok(Self::Static { spot_usd_cross: usd })
            }
            SpotConfig::Pyth {
                max_publish_lag_ms,
                max_conf_bps,
            } => {
                let underlying_feed = underlying_spec
                    .pyth_feed()
                    .context("resolving pyth feed for underlying")?;
                let settlement_feed = settlement_spec
                    .pyth_feed()
                    .context("resolving pyth feed for settlement")?;
                Ok(Self::Pyth {
                    underlying_feed,
                    settlement_feed,
                    max_publish_lag_ms,
                    max_conf_bps,
                })
            }
        }
    }

    /// Resolve the current USD cross. Static returns immediately; Pyth reads
    /// both legs from oracle-service.
    pub async fn resolve_usd_cross(&self, oracle: &OracleClient) -> Result<f64> {
        match *self {
            Self::Static { spot_usd_cross } => Ok(spot_usd_cross),
            Self::Pyth {
                underlying_feed,
                settlement_feed,
                max_publish_lag_ms,
                max_conf_bps,
            } => {
                let u = oracle.price(underlying_feed).await.map_err(|_| {
                    SpotError::MissingFeed {
                        leg: "underlying",
                        feed_id: underlying_feed.to_hex(),
                    }
                })?;
                let s = oracle.price(settlement_feed).await.map_err(|_| {
                    SpotError::MissingFeed {
                        leg: "settlement",
                        feed_id: settlement_feed.to_hex(),
                    }
                })?;
                let cross = compute_usd_cross(&u, &s, max_publish_lag_ms, max_conf_bps)?;
                info!(
                    underlying_usd = u.price,
                    settlement_usd = s.price,
                    cross,
                    "pyth spot resolved"
                );
                Ok(cross)
            }
        }
    }
}

/// Validate both legs and return the USD cross (settlement-per-underlying).
/// All chain-unit conversion happens downstream in `strike_grid`.
pub fn compute_usd_cross(
    underlying: &PricePoint,
    settlement: &PricePoint,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<f64, SpotError> {
    validate_leg("underlying", underlying, max_publish_lag_ms, max_conf_bps)?;
    validate_leg("settlement", settlement, max_publish_lag_ms, max_conf_bps)?;
    let cross = underlying.price / settlement.price;
    if !cross.is_finite() || cross <= 0.0 {
        return Err(SpotError::OutOfRange { scaled: cross });
    }
    Ok(cross)
}

fn validate_leg(
    leg: &'static str,
    p: &PricePoint,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<(), SpotError> {
    if !p.price.is_finite() || p.price <= 0.0 {
        return Err(SpotError::BadPrice { leg, price: p.price });
    }
    let conf_bps = if p.price > 0.0 {
        p.conf / p.price * 10_000.0
    } else {
        f64::INFINITY
    };
    if conf_bps > max_conf_bps as f64 {
        return Err(SpotError::WideConfidence {
            leg,
            conf_bps,
            max_bps: max_conf_bps,
        });
    }
    if p.age_ms > max_publish_lag_ms as i64 {
        return Err(SpotError::StalePrice {
            leg,
            age_ms: p.age_ms,
            max_ms: max_publish_lag_ms,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a price point with an explicit publisher age (ms).
    fn point(price: f64, conf: f64, age_ms: i64) -> PricePoint {
        PricePoint {
            feed_id: "00".repeat(32),
            price,
            conf,
            publish_time_ms: 0,
            age_ms,
        }
    }

    #[test]
    fn btc_usdc_50k_cross_is_50k() {
        let u = point(50_000.0, 0.1, 0);
        let s = point(1.0, 0.0001, 0);
        let cross = compute_usd_cross(&u, &s, 30_000, 100).unwrap();
        assert!((cross - 50_000.0).abs() < 1e-6);
    }

    #[test]
    fn sub_dollar_cross_is_exact() {
        let u = point(0.15, 0.00001, 0);
        let s = point(1.0, 0.0001, 0);
        let cross = compute_usd_cross(&u, &s, 30_000, 100).unwrap();
        assert!((cross - 0.15).abs() < 1e-12);
    }

    #[test]
    fn stale_underlying_rejected() {
        let u = point(50_000.0, 0.1, 60_000);
        let s = point(1.0, 0.0001, 0);
        let err = compute_usd_cross(&u, &s, 30_000, 100).unwrap_err();
        match err {
            SpotError::StalePrice { leg, age_ms, .. } => {
                assert_eq!(leg, "underlying");
                assert!(age_ms >= 60_000, "age_ms = {age_ms}");
            }
            other => panic!("expected StalePrice, got {other:?}"),
        }
    }

    #[test]
    fn stale_settlement_rejected() {
        let u = point(50_000.0, 0.1, 0);
        let s = point(1.0, 0.0001, 5 * 60_000);
        let err = compute_usd_cross(&u, &s, 30_000, 100).unwrap_err();
        assert!(
            matches!(err, SpotError::StalePrice { leg: "settlement", .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn wide_confidence_rejected() {
        // 5% confidence on the underlying — way above the 100 bps default.
        let u = point(50_000.0, 2_500.0, 0);
        let s = point(1.0, 0.0001, 0);
        let err = compute_usd_cross(&u, &s, 30_000, 100).unwrap_err();
        match err {
            SpotError::WideConfidence {
                leg,
                conf_bps,
                max_bps,
            } => {
                assert_eq!(leg, "underlying");
                assert_eq!(max_bps, 100);
                assert!(conf_bps > 100.0, "conf_bps = {conf_bps}");
            }
            other => panic!("expected WideConfidence, got {other:?}"),
        }
    }

    #[test]
    fn non_positive_price_rejected() {
        let u = point(0.0, 1.0, 0);
        let s = point(1.0, 0.0001, 0);
        let err = compute_usd_cross(&u, &s, 30_000, 100).unwrap_err();
        assert!(matches!(err, SpotError::BadPrice { leg: "underlying", .. }));
    }
}
