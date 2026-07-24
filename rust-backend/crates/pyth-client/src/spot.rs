//! Spot construction from a pair of Pyth USD feeds, with staleness /
//! confidence gating. Lifted out of mm-bot's `pricing` module (SO-302) so
//! the market-sim service can price its spot bands off the same math the
//! desk quotes with.

use std::time::Duration;

use crate::cache::PriceCache;
use crate::types::PriceFeedId;

/// Freshness/quality bounds applied when reading Pyth's cache.
#[derive(Clone, Copy, Debug)]
pub struct Staleness {
    /// Maximum age of our local observation of a price.
    pub max_price_age: Duration,
    /// Maximum lag between Pyth's publisher timestamp and `now`.
    pub max_publish_lag: Duration,
    /// Maximum Pyth confidence interval, as basis points of the price:
    /// decline instead of quoting off a feed that is fresh but unsure of
    /// itself. 0 disables the check.
    pub max_conf_bps: u64,
}

/// Reasons `compute_spot*` can fail. Stable strings so callers can
/// forward them straight into a decline message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotError {
    UnderlyingStale,
    SettlementStale,
    NonPositivePrice,
    OutOfRange,
    ConfidenceTooWide,
}

impl SpotError {
    pub fn as_str(self) -> &'static str {
        match self {
            SpotError::UnderlyingStale => "underlying price stale or unseen",
            SpotError::SettlementStale => "settlement price stale or unseen",
            SpotError::NonPositivePrice => "non-positive or non-finite price",
            SpotError::OutOfRange => "scaled spot out of range",
            SpotError::ConfidenceTooWide => "price confidence too wide",
        }
    }
}

/// Cross USD/USD → settlement-asset-raw-units per underlying-asset-raw-unit.
///
/// Equivalent to `(underlying_usd / settlement_usd) * 10^(settle_dec - under_dec)`.
///
/// Returned as `f64`, NOT rounded to an integer: for a sub-dollar underlying
/// (DEEP ≈ $0.016) against an equal-decimals settlement the ratio is well below
/// 1 (e.g. 0.016 settlement-raw per underlying-raw), so rounding to a u64 would
/// collapse it to 0 and price every such option to zero.
pub fn compute_spot_from_prices(
    underlying_usd: f64,
    settlement_usd: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<f64, SpotError> {
    if !(underlying_usd.is_finite()
        && underlying_usd > 0.0
        && settlement_usd.is_finite()
        && settlement_usd > 0.0)
    {
        return Err(SpotError::NonPositivePrice);
    }
    let cross = underlying_usd / settlement_usd;
    let scale = 10f64.powi(settlement_decimals as i32 - underlying_decimals as i32);
    let scaled = cross * scale;
    // Reject non-finite / negative, and absurdly large ratios (a spot needing
    // more than u64::MAX settlement-raw per underlying-raw is not a real market).
    if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
        return Err(SpotError::OutOfRange);
    }
    Ok(scaled)
}

/// Same as [`compute_spot_from_prices`] but reads both feeds out of a
/// [`PriceCache`], applying staleness and confidence bounds.
pub fn compute_spot_from_cache(
    cache: &PriceCache,
    underlying_feed: PriceFeedId,
    settlement_feed: PriceFeedId,
    underlying_decimals: u8,
    settlement_decimals: u8,
    staleness: Staleness,
) -> Result<f64, SpotError> {
    let u = cache
        .get_fresh(underlying_feed, staleness.max_price_age, staleness.max_publish_lag)
        .ok_or(SpotError::UnderlyingStale)?;
    let s = cache
        .get_fresh(settlement_feed, staleness.max_price_age, staleness.max_publish_lag)
        .ok_or(SpotError::SettlementStale)?;
    if staleness.max_conf_bps > 0 {
        // Pyth's conf is the 1-sigma uncertainty of the aggregate price —
        // exactly the moments (thin books, publisher disagreement) a quote
        // is most likely to be picked off.
        let limit = staleness.max_conf_bps as f64 / 10_000.0;
        for cp in [&u, &s] {
            if cp.price > 0.0 && cp.conf.is_finite() && cp.conf > cp.price * limit {
                return Err(SpotError::ConfidenceTooWide);
            }
        }
    }
    compute_spot_from_prices(u.price, s.price, underlying_decimals, settlement_decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use crate::cache::CachedPrice;

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    // -- compute_spot_from_prices ---------------------------------------

    #[test]
    fn spot_btc_usdc_same_decimals() {
        // BTC = $60_000, USDC = $1.0, both 8d → spot = 60_000 * 10^0 = 60_000
        let s = compute_spot_from_prices(60_000.0, 1.0, 8, 8).unwrap();
        close(s, 60_000.0, 1e-9);
    }

    #[test]
    fn spot_scales_by_decimal_delta() {
        // Underlying 8d, settlement 6d → 10^(6-8) = 0.01 → spot 60_000 * 0.01 = 600
        let s = compute_spot_from_prices(60_000.0, 1.0, 8, 6).unwrap();
        close(s, 600.0, 1e-9);
        // Other way around: 10^(8-6) = 100 → spot 60_000 * 100 = 6_000_000
        let s = compute_spot_from_prices(60_000.0, 1.0, 6, 8).unwrap();
        close(s, 6_000_000.0, 1e-6);
    }

    #[test]
    fn spot_sub_unit_ratio_keeps_precision() {
        // Regression: DEEP ≈ $0.0158 against TUSDC, both 6d → ratio 0.0158
        // settlement-raw per underlying-raw. The old u64 round collapsed this
        // to 0 (every DEEP option priced to zero); f64 keeps it.
        let s = compute_spot_from_prices(0.0158, 1.0, 6, 6).unwrap();
        close(s, 0.0158, 1e-12);
        // TWAL ≈ $0.0327, 9d underlying vs 6d settlement → 0.0327 * 10^-3.
        let s = compute_spot_from_prices(0.0327, 1.0, 9, 6).unwrap();
        close(s, 3.27e-5, 1e-15);
    }

    #[test]
    fn spot_non_dollar_settlement() {
        // Underlying $60_000, settlement $1500 (e.g. ETH-quoted): spot ≈ 40
        let s = compute_spot_from_prices(60_000.0, 1_500.0, 8, 8).unwrap();
        close(s, 40.0, 1e-9);
    }

    #[test]
    fn spot_rejects_non_positive() {
        assert_eq!(
            compute_spot_from_prices(0.0, 1.0, 8, 8).unwrap_err(),
            SpotError::NonPositivePrice
        );
        assert_eq!(
            compute_spot_from_prices(-1.0, 1.0, 8, 8).unwrap_err(),
            SpotError::NonPositivePrice
        );
        assert_eq!(
            compute_spot_from_prices(60_000.0, 0.0, 8, 8).unwrap_err(),
            SpotError::NonPositivePrice
        );
        assert_eq!(
            compute_spot_from_prices(f64::NAN, 1.0, 8, 8).unwrap_err(),
            SpotError::NonPositivePrice
        );
        assert_eq!(
            compute_spot_from_prices(f64::INFINITY, 1.0, 8, 8).unwrap_err(),
            SpotError::NonPositivePrice
        );
    }

    #[test]
    fn spot_overflow_is_rejected() {
        // 1e30 / 1.0 * 10^0 → way past u64::MAX (~1.8e19).
        let e = compute_spot_from_prices(1e30, 1.0, 8, 8).unwrap_err();
        assert_eq!(e, SpotError::OutOfRange);
    }

    #[test]
    fn spot_error_messages_are_stable() {
        // The decline path forwards these verbatim into the WS reason — pin them.
        assert_eq!(SpotError::UnderlyingStale.as_str(), "underlying price stale or unseen");
        assert_eq!(SpotError::SettlementStale.as_str(), "settlement price stale or unseen");
        assert_eq!(SpotError::NonPositivePrice.as_str(), "non-positive or non-finite price");
        assert_eq!(SpotError::OutOfRange.as_str(), "scaled spot out of range");
        assert_eq!(SpotError::ConfidenceTooWide.as_str(), "price confidence too wide");
    }

    // -- compute_spot_from_cache ----------------------------------------

    fn feed_id(byte: u8) -> PriceFeedId {
        PriceFeedId([byte; 32])
    }

    fn cached(price: f64) -> CachedPrice {
        // publish_time_ms = `now` in millis so the publish-lag check passes.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        CachedPrice {
            price,
            conf: 0.0,
            publish_time_ms: now_ms,
            observed_at: Instant::now(),
        }
    }

    fn loose_staleness() -> Staleness {
        Staleness {
            max_price_age: Duration::from_secs(60),
            max_publish_lag: Duration::from_secs(60),
            max_conf_bps: 0,
        }
    }

    #[test]
    fn cache_spot_happy_path() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(u, cached(60_000.0));
        cache.insert(s, cached(1.0));
        let spot = compute_spot_from_cache(&cache, u, s, 8, 8, loose_staleness()).unwrap();
        close(spot, 60_000.0, 1e-9);
    }

    #[test]
    fn cache_spot_underlying_missing_is_stale() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(s, cached(1.0));
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, loose_staleness()).unwrap_err();
        assert_eq!(e, SpotError::UnderlyingStale);
    }

    #[test]
    fn cache_spot_settlement_missing_is_stale() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(u, cached(60_000.0));
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, loose_staleness()).unwrap_err();
        assert_eq!(e, SpotError::SettlementStale);
    }

    #[test]
    fn cache_spot_rejects_old_local_observation() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        // observed_at far in the past → exceeds max_price_age.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale = CachedPrice {
            price: 60_000.0,
            conf: 0.0,
            publish_time_ms: now_ms,
            observed_at: Instant::now() - Duration::from_secs(120),
        };
        cache.insert(u, stale);
        cache.insert(s, cached(1.0));
        let staleness = Staleness {
            max_price_age: Duration::from_secs(60),
            max_publish_lag: Duration::from_secs(600),
            max_conf_bps: 0,
        };
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, staleness).unwrap_err();
        assert_eq!(e, SpotError::UnderlyingStale);
    }

    #[test]
    fn cache_spot_rejects_old_publisher_time() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // publish_time_ms 10 minutes in the past → exceeds max_publish_lag.
        cache.insert(
            u,
            CachedPrice {
                price: 60_000.0,
                conf: 0.0,
                publish_time_ms: now_ms - 10 * 60 * 1000,
                observed_at: Instant::now(),
            },
        );
        cache.insert(s, cached(1.0));
        let staleness = Staleness {
            max_price_age: Duration::from_secs(600),
            max_publish_lag: Duration::from_secs(60),
            max_conf_bps: 0,
        };
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, staleness).unwrap_err();
        assert_eq!(e, SpotError::UnderlyingStale);
    }

    #[test]
    fn cache_spot_forwards_non_positive_through() {
        // Both feeds present and fresh, but underlying price is zero —
        // the math layer (compute_spot_from_prices) should reject.
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(u, cached(0.0));
        cache.insert(s, cached(1.0));
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, loose_staleness()).unwrap_err();
        assert_eq!(e, SpotError::NonPositivePrice);
    }

    fn cached_with_conf(price: f64, conf: f64) -> CachedPrice {
        CachedPrice { conf, ..cached(price) }
    }

    #[test]
    fn cache_spot_rejects_wide_confidence() {
        // Underlying conf = 5% of price; a 100 bps cap must decline it.
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(u, cached_with_conf(60_000.0, 3_000.0));
        cache.insert(s, cached(1.0));
        let guarded = Staleness { max_conf_bps: 100, ..loose_staleness() };
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, guarded).unwrap_err();
        assert_eq!(e, SpotError::ConfidenceTooWide);
        // The settlement leg is guarded too.
        let cache = PriceCache::new();
        cache.insert(u, cached(60_000.0));
        cache.insert(s, cached_with_conf(1.0, 0.05));
        let e = compute_spot_from_cache(&cache, u, s, 8, 8, guarded).unwrap_err();
        assert_eq!(e, SpotError::ConfidenceTooWide);
    }

    #[test]
    fn cache_spot_conf_gate_disabled_and_within_bounds_pass() {
        let cache = PriceCache::new();
        let u = feed_id(0x01);
        let s = feed_id(0x02);
        cache.insert(u, cached_with_conf(60_000.0, 3_000.0));
        cache.insert(s, cached(1.0));
        // max_conf_bps = 0 disables the check entirely.
        assert!(compute_spot_from_cache(&cache, u, s, 8, 8, loose_staleness()).is_ok());
        // Conf inside the cap passes: 30 (5 bps) under a 100 bps cap.
        cache.insert(u, cached_with_conf(60_000.0, 30.0));
        let guarded = Staleness { max_conf_bps: 100, ..loose_staleness() };
        assert!(compute_spot_from_cache(&cache, u, s, 8, 8, guarded).is_ok());
    }
}
