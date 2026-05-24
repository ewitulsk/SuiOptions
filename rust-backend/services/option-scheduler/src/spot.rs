//! Spot price source.
//!
//! Two variants today:
//!
//! - `static` — a hardcoded USD price from `config.toml`. Useful for
//!   tests, offline dry-runs, and pairs whose price genuinely doesn't
//!   move (a peg test).
//! - `pyth` — live USD-cross via Pyth Hermes. Pulls the latest cached
//!   price for both legs in one HTTP call, validates each one against the
//!   configured staleness / confidence guards, and returns the cross
//!   pre-scaled into the bucket's u64 strike unit.
//!
//! Both variants return the same thing: the strike-grid "spot" in chain
//! units (settlement smallest-units per underlying smallest-unit). The
//! strike-grid module never sees USD; it operates entirely on the u64
//! the resolver hands back.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use thiserror::Error;
use tracing::info;

use pyth_client::types::{PriceFeedId, PriceUpdate, PythPrice};
use pyth_client as pyth;

use crate::config::SpotConfig;
use crate::strike_grid::spot_usd_to_chain_units;

/// Spot source with all chain-relevant inputs already resolved against
/// `deployments.json`. Built once at boot per pair so a Pyth misconfig
/// (missing `pythFeedId`, malformed hex) fails before the first tick.
#[derive(Debug, Clone)]
pub enum ResolvedSpotSource {
    Static {
        /// Spot scaled into chain units at boot. Doesn't change over the
        /// life of the process, so we precompute it once.
        spot_chain: u64,
    },
    Pyth {
        underlying_feed: PriceFeedId,
        settlement_feed: PriceFeedId,
        /// Reject a roll if either feed's `publish_time` is older than
        /// this. The strike grid only updates weekly, so we can afford to
        /// be strict here without hurting availability.
        max_publish_lag_ms: u64,
        /// Reject a roll if `conf / price` exceeds this on either leg.
        /// Bps so the threshold reads naturally next to fee configs.
        max_conf_bps: u32,
        underlying_decimals: u8,
        settlement_decimals: u8,
    },
}

/// Structured reasons why a Pyth resolve declined to produce a spot.
/// Modeled as an enum so tests can match on the variant and the tick
/// loop can log a concise reason field instead of an opaque string.
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

    #[error("scaled spot is out of u64 range or non-finite: {scaled}")]
    OutOfRange { scaled: f64 },
}

impl ResolvedSpotSource {
    /// Build a resolved source from the per-pair config + the two token
    /// catalog entries. The Pyth path requires both legs to carry a
    /// `pythFeedId` in `deployments.json`; missing either one fails at
    /// boot.
    pub fn from_config(
        cfg: &SpotConfig,
        underlying_spec: &deployments::TokenSpec,
        settlement_spec: &deployments::TokenSpec,
    ) -> Result<Self> {
        match *cfg {
            SpotConfig::Static { usd } => {
                let spot_chain = spot_usd_to_chain_units(
                    usd,
                    underlying_spec.decimals,
                    settlement_spec.decimals,
                )?;
                Ok(Self::Static { spot_chain })
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
                    underlying_decimals: underlying_spec.decimals,
                    settlement_decimals: settlement_spec.decimals,
                })
            }
        }
    }

    /// Resolve the current chain-unit spot. Static returns immediately;
    /// Pyth issues one HTTP call to Hermes.
    pub async fn resolve_chain_units(
        &self,
        client: &reqwest::Client,
        hermes_url: &str,
    ) -> Result<u64> {
        match *self {
            Self::Static { spot_chain } => Ok(spot_chain),
            Self::Pyth {
                underlying_feed,
                settlement_feed,
                max_publish_lag_ms,
                max_conf_bps,
                underlying_decimals,
                settlement_decimals,
            } => {
                let updates =
                    pyth::http::latest(client, hermes_url, &[underlying_feed, settlement_feed])
                        .await
                        .context("fetching latest pyth prices")?;
                let now_ms = current_time_ms();
                let u = find_price(&updates, underlying_feed).ok_or_else(|| {
                    SpotError::MissingFeed {
                        leg: "underlying",
                        feed_id: underlying_feed.to_hex(),
                    }
                })?;
                let s = find_price(&updates, settlement_feed).ok_or_else(|| {
                    SpotError::MissingFeed {
                        leg: "settlement",
                        feed_id: settlement_feed.to_hex(),
                    }
                })?;
                let chain = compute_chain_spot(
                    &u.price,
                    &s.price,
                    underlying_decimals,
                    settlement_decimals,
                    max_publish_lag_ms,
                    max_conf_bps,
                    now_ms,
                )?;
                info!(
                    underlying_usd = u.price.price_f64().unwrap_or(f64::NAN),
                    settlement_usd = s.price.price_f64().unwrap_or(f64::NAN),
                    chain_spot = chain,
                    "pyth spot resolved"
                );
                Ok(chain)
            }
        }
    }
}

fn find_price(updates: &[PriceUpdate], feed: PriceFeedId) -> Option<&PriceUpdate> {
    let target = feed.to_hex();
    updates.iter().find(|u| u.id.eq_ignore_ascii_case(&target))
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Validate both Pyth legs and return the cross-price in chain units.
///
/// Kept pure (no I/O) so the tests can drive it with hand-built
/// `PythPrice` fixtures. Matches the convention `mm-bot::compute_spot`
/// uses — settlement smallest-units per underlying smallest-unit —
/// so a future shared helper can unify them.
pub fn compute_chain_spot(
    underlying: &PythPrice,
    settlement: &PythPrice,
    underlying_decimals: u8,
    settlement_decimals: u8,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
    now_ms: i64,
) -> Result<u64, SpotError> {
    validate_leg("underlying", underlying, max_publish_lag_ms, max_conf_bps, now_ms)?;
    validate_leg("settlement", settlement, max_publish_lag_ms, max_conf_bps, now_ms)?;

    // Both prices passed validation so the f64 conversions succeed.
    let u_price = underlying.price_f64().map_err(|_| SpotError::BadPrice {
        leg: "underlying",
        price: f64::NAN,
    })?;
    let s_price = settlement.price_f64().map_err(|_| SpotError::BadPrice {
        leg: "settlement",
        price: f64::NAN,
    })?;
    let cross = u_price / s_price;
    let scale = 10f64.powi(settlement_decimals as i32 - underlying_decimals as i32);
    let scaled = cross * scale;
    if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
        return Err(SpotError::OutOfRange { scaled });
    }
    Ok(scaled.round() as u64)
}

fn validate_leg(
    leg: &'static str,
    p: &PythPrice,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
    now_ms: i64,
) -> Result<(), SpotError> {
    let price = p.price_f64().map_err(|_| SpotError::BadPrice {
        leg,
        price: f64::NAN,
    })?;
    if !price.is_finite() || price <= 0.0 {
        return Err(SpotError::BadPrice { leg, price });
    }
    let conf = p.conf_f64().map_err(|_| SpotError::BadPrice {
        leg,
        price: f64::NAN,
    })?;
    let conf_bps = if price > 0.0 { conf / price * 10_000.0 } else { f64::INFINITY };
    if conf_bps > max_conf_bps as f64 {
        return Err(SpotError::WideConfidence {
            leg,
            conf_bps,
            max_bps: max_conf_bps,
        });
    }
    let age_ms = now_ms - p.publish_time_ms();
    if age_ms > max_publish_lag_ms as i64 {
        return Err(SpotError::StalePrice {
            leg,
            age_ms,
            max_ms: max_publish_lag_ms,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        // Fixed reference time so the `publish_time` deltas are
        // deterministic.
        1_750_000_000_000
    }

    fn price(price: i64, conf: u64, expo: i32, publish_ms: i64) -> PythPrice {
        PythPrice {
            price: price.to_string(),
            conf: conf.to_string(),
            expo,
            publish_time: publish_ms / 1000,
        }
    }

    #[test]
    fn btc_usdc_50k_cross_matches_static_fixture() {
        // Pyth shape: BTC at $50_000 with expo=-8 → integer 50_000 * 10^8.
        let u = price(50_000 * 100_000_000, 10_000_000, -8, now_ms());
        // USDC at $1, expo=-8 → 1 * 10^8 = 100_000_000.
        let s = price(100_000_000, 10_000, -8, now_ms());
        let chain = compute_chain_spot(&u, &s, 8, 6, 30_000, 100, now_ms()).unwrap();
        assert_eq!(chain, 500, "BTC/USDC cross at $50k should land on 500");
    }

    #[test]
    fn deep_at_high_settle_precision() {
        // DEEP at $0.15, USDC at $1, 6-dec underlying, 9-dec settlement.
        // Expected: 0.15 × 10^3 = 150.
        let u = price(15_000_000, 1_000, -8, now_ms());
        let s = price(100_000_000, 10_000, -8, now_ms());
        let chain = compute_chain_spot(&u, &s, 6, 9, 30_000, 100, now_ms()).unwrap();
        assert_eq!(chain, 150);
    }

    #[test]
    fn stale_underlying_rejected() {
        let u = price(50_000 * 100_000_000, 10_000_000, -8, now_ms() - 60_000);
        let s = price(100_000_000, 10_000, -8, now_ms());
        let err = compute_chain_spot(&u, &s, 8, 6, 30_000, 100, now_ms()).unwrap_err();
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
        let u = price(50_000 * 100_000_000, 10_000_000, -8, now_ms());
        let s = price(100_000_000, 10_000, -8, now_ms() - 5 * 60_000);
        let err = compute_chain_spot(&u, &s, 8, 6, 30_000, 100, now_ms()).unwrap_err();
        assert!(
            matches!(err, SpotError::StalePrice { leg: "settlement", .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn wide_confidence_rejected() {
        // 5% confidence on the underlying — way above the 100 bps default.
        let u = price(50_000 * 100_000_000, 2_500 * 100_000_000, -8, now_ms());
        let s = price(100_000_000, 10_000, -8, now_ms());
        let err = compute_chain_spot(&u, &s, 8, 6, 30_000, 100, now_ms()).unwrap_err();
        match err {
            SpotError::WideConfidence {
                leg, conf_bps, max_bps,
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
        let u = price(0, 1, -8, now_ms());
        let s = price(100_000_000, 10_000, -8, now_ms());
        let err = compute_chain_spot(&u, &s, 8, 6, 30_000, 100, now_ms()).unwrap_err();
        assert!(matches!(err, SpotError::BadPrice { leg: "underlying", .. }));
    }
}
