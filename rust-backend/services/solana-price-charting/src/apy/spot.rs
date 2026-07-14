//! Spot-cross resolution against solana-oracle-service.
//!
//! The USD-cross math (per-leg confidence + publish-lag guards, then divide)
//! is unchanged from the Sui twin — Pyth feed ids are chain-agnostic, so the
//! only Solana difference is that the token catalog entries come from
//! solana-token-info (keyed by SPL mint instead of coin type).

use anyhow::{Context, Result};
use thiserror::Error;

use oracle_client::{OracleClient, PricePoint};
use solana_token_info_client::SupportedToken;

#[derive(Debug, Error, PartialEq)]
pub enum SpotError {
    #[error("missing pyth price for {leg} leg")]
    MissingFeed { leg: &'static str },
    #[error("non-positive or non-finite {leg} price: {price}")]
    BadPrice { leg: &'static str, price: f64 },
    #[error("{leg} publish_time is stale: {age_ms}ms old, max {max_ms}ms")]
    StalePrice { leg: &'static str, age_ms: i64, max_ms: u64 },
    #[error("{leg} confidence too wide: {conf_bps:.2} bps, max {max_bps} bps")]
    WideConfidence { leg: &'static str, conf_bps: f64, max_bps: u32 },
    #[error("cross is out of range or non-finite: {cross}")]
    OutOfRange { cross: f64 },
}

/// Resolve the USD cross (settlement units per 1 underlying) for a vault's
/// pair via oracle-service. Both tokens must carry a `pyth_feed_id` in the
/// token-info catalog.
pub async fn resolve_cross(
    oracle: &OracleClient,
    underlying: &SupportedToken,
    settlement: &SupportedToken,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<f64> {
    let uf = underlying.pyth_feed().context("underlying pyth feed")?;
    let sf = settlement.pyth_feed().context("settlement pyth feed")?;
    let u = oracle
        .price(uf)
        .await
        .map_err(|_| SpotError::MissingFeed { leg: "underlying" })?;
    let s = oracle
        .price(sf)
        .await
        .map_err(|_| SpotError::MissingFeed { leg: "settlement" })?;
    let cross = compute_cross(&u, &s, max_publish_lag_ms, max_conf_bps)?;
    Ok(cross)
}

/// Validate both legs and return `underlying_usd / settlement_usd`.
pub fn compute_cross(
    underlying: &PricePoint,
    settlement: &PricePoint,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<f64, SpotError> {
    validate_point("underlying", underlying, max_publish_lag_ms, max_conf_bps)?;
    validate_point("settlement", settlement, max_publish_lag_ms, max_conf_bps)?;
    let cross = underlying.price / settlement.price;
    if !cross.is_finite() || cross <= 0.0 {
        return Err(SpotError::OutOfRange { cross });
    }
    Ok(cross)
}

fn validate_point(
    leg: &'static str,
    p: &PricePoint,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<(), SpotError> {
    if !p.price.is_finite() || p.price <= 0.0 {
        return Err(SpotError::BadPrice { leg, price: p.price });
    }
    let conf_bps = if p.price > 0.0 { p.conf / p.price * 10_000.0 } else { f64::INFINITY };
    if conf_bps > max_conf_bps as f64 {
        return Err(SpotError::WideConfidence { leg, conf_bps, max_bps: max_conf_bps });
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
