//! Pyth spot-cross + realized-vol resolution.
//!
//! The USD-cross math is lifted from `option-scheduler::spot` (a binary crate,
//! so not importable). Both call `pyth_client::latest` for the two legs and
//! `pyth_client::sigma::realized_sigma_from_benchmarks` for vol. A future
//! `crates/market-data` extraction would let both share one copy; kept local
//! here to avoid churning the scheduler.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use thiserror::Error;

use pyth_client::types::{PriceUpdate, PythPrice};
use token_info_client::SupportedToken;

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
/// pair via Pyth Hermes. Both tokens must carry a `pyth_feed_id` in the
/// token-info catalog.
pub async fn resolve_cross(
    http: &reqwest::Client,
    hermes_url: &str,
    underlying: &SupportedToken,
    settlement: &SupportedToken,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
) -> Result<f64> {
    let uf = underlying.pyth_feed().context("underlying pyth feed")?;
    let sf = settlement.pyth_feed().context("settlement pyth feed")?;
    let updates = pyth_client::latest(http, hermes_url, &[uf, sf])
        .await
        .context("fetching latest pyth prices")?;
    let u = find_price(&updates, &uf.to_hex())
        .ok_or(SpotError::MissingFeed { leg: "underlying" })?;
    let s = find_price(&updates, &sf.to_hex())
        .ok_or(SpotError::MissingFeed { leg: "settlement" })?;
    let cross = compute_cross(&u.price, &s.price, max_publish_lag_ms, max_conf_bps, now_ms())?;
    Ok(cross)
}

/// The stable Benchmarks feed id for an underlying's realized-vol lookup.
/// Benchmarks only serves the stable feed set, so the (beta) feed is mapped
/// to its stable equivalent; realized vol then uses the real asset's history.
pub fn benchmark_feed(underlying: &SupportedToken) -> Result<pyth_client::PriceFeedId> {
    Ok(pyth_client::benchmark_feed_id(
        underlying.pyth_feed().context("underlying pyth feed")?,
    ))
}

fn find_price<'a>(updates: &'a [PriceUpdate], id_hex: &str) -> Option<&'a PriceUpdate> {
    updates.iter().find(|u| u.id.eq_ignore_ascii_case(id_hex))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Validate both legs and return `underlying_usd / settlement_usd`.
pub fn compute_cross(
    underlying: &PythPrice,
    settlement: &PythPrice,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
    now_ms: i64,
) -> Result<f64, SpotError> {
    validate_leg("underlying", underlying, max_publish_lag_ms, max_conf_bps, now_ms)?;
    validate_leg("settlement", settlement, max_publish_lag_ms, max_conf_bps, now_ms)?;
    let u = underlying
        .price_f64()
        .map_err(|_| SpotError::BadPrice { leg: "underlying", price: f64::NAN })?;
    let s = settlement
        .price_f64()
        .map_err(|_| SpotError::BadPrice { leg: "settlement", price: f64::NAN })?;
    let cross = u / s;
    if !cross.is_finite() || cross <= 0.0 {
        return Err(SpotError::OutOfRange { cross });
    }
    Ok(cross)
}

fn validate_leg(
    leg: &'static str,
    p: &PythPrice,
    max_publish_lag_ms: u64,
    max_conf_bps: u32,
    now_ms: i64,
) -> Result<(), SpotError> {
    let price = p
        .price_f64()
        .map_err(|_| SpotError::BadPrice { leg, price: f64::NAN })?;
    if !price.is_finite() || price <= 0.0 {
        return Err(SpotError::BadPrice { leg, price });
    }
    let conf = p
        .conf_f64()
        .map_err(|_| SpotError::BadPrice { leg, price: f64::NAN })?;
    let conf_bps = if price > 0.0 { conf / price * 10_000.0 } else { f64::INFINITY };
    if conf_bps > max_conf_bps as f64 {
        return Err(SpotError::WideConfidence { leg, conf_bps, max_bps: max_conf_bps });
    }
    let age_ms = now_ms - p.publish_time_ms();
    if age_ms > max_publish_lag_ms as i64 {
        return Err(SpotError::StalePrice { leg, age_ms, max_ms: max_publish_lag_ms });
    }
    Ok(())
}
