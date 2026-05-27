//! Pure helpers that drive the bot's per-RFQ decision.
//!
//! These functions are deliberately decoupled from IO so the entire
//! market-making flow can be unit-tested: spot construction from Pyth
//! prices, staleness checks, strike rescaling, time-to-expiry, vol
//! selection with a fallback, Black-Scholes valuation, and the final
//! quote/decline decision.

use std::time::Duration;

use pricing::{call_price_per_unit, premium_for_write, CallInputs};
use protocol_types::messages::RfqBroadcastPayload;
use pyth_client::{PriceCache, PriceFeedId};

/// Knobs that affect quote arithmetic but are independent of staleness/IO.
#[derive(Clone, Copy, Debug)]
pub struct PricingConfig {
    /// Annualized risk-free rate, continuous compounding.
    pub rate: f64,
    /// How long the quote we emit stays valid, in milliseconds.
    pub quote_ttl_ms: u64,
}

/// Staleness bounds applied when reading Pyth's cache.
#[derive(Clone, Copy, Debug)]
pub struct Staleness {
    /// Maximum age of our local observation of a price.
    pub max_price_age: Duration,
    /// Maximum lag between Pyth's publisher timestamp and `now`.
    pub max_publish_lag: Duration,
}

/// Reasons `compute_spot*` can fail. Stable strings so callers can
/// forward them straight into a decline message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotError {
    UnderlyingStale,
    SettlementStale,
    NonPositivePrice,
    OutOfRange,
}

impl SpotError {
    pub fn as_str(self) -> &'static str {
        match self {
            SpotError::UnderlyingStale => "underlying price stale or unseen",
            SpotError::SettlementStale => "settlement price stale or unseen",
            SpotError::NonPositivePrice => "non-positive or non-finite price",
            SpotError::OutOfRange => "scaled spot out of range",
        }
    }
}

/// Outcome of pricing one RFQ.
#[derive(Clone, Debug, PartialEq)]
pub enum PriceDecision {
    Quote {
        /// Premium in settlement smallest-units.
        premium: u64,
        /// When the quote expires; absolute Unix ms.
        valid_until_ms: u64,
        /// Inputs that produced the price — convenient for logs/tests.
        spot_scaled: u64,
        strike_scaled: f64,
        t_years: f64,
        sigma: f64,
        per_unit: f64,
    },
    Decline {
        reason: String,
    },
}

/// Cross USD/USD → settlement-asset-raw-units per underlying-asset-raw-unit.
///
/// Equivalent to: `(underlying_usd / settlement_usd) * 10^(settle_dec - under_dec)`,
/// rounded to the nearest u64.
pub fn compute_spot_from_prices(
    underlying_usd: f64,
    settlement_usd: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<u64, SpotError> {
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
    if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
        return Err(SpotError::OutOfRange);
    }
    Ok(scaled.round() as u64)
}

/// Same as [`compute_spot_from_prices`] but reads both feeds out of a
/// [`PriceCache`], applying staleness bounds.
pub fn compute_spot_from_cache(
    cache: &PriceCache,
    underlying_feed: PriceFeedId,
    settlement_feed: PriceFeedId,
    underlying_decimals: u8,
    settlement_decimals: u8,
    staleness: Staleness,
) -> Result<u64, SpotError> {
    let u = cache
        .get_fresh(underlying_feed, staleness.max_price_age, staleness.max_publish_lag)
        .ok_or(SpotError::UnderlyingStale)?;
    let s = cache
        .get_fresh(settlement_feed, staleness.max_price_age, staleness.max_publish_lag)
        .ok_or(SpotError::SettlementStale)?;
    compute_spot_from_prices(u.price, s.price, underlying_decimals, settlement_decimals)
}

/// Time to expiry in years, saturating at zero so an already-expired
/// bucket prices to intrinsic.
pub fn time_to_expiry_years(expiry_ms: u64, now_ms: u64) -> f64 {
    let ms = expiry_ms.saturating_sub(now_ms);
    ms as f64 / 1000.0 / 86_400.0 / 365.0
}

/// Convert the bucket's `(strike, strike_scale)` pair back to a strike at
/// scale=0 (settlement raw-units per underlying raw-unit), for plugging
/// into Black-Scholes alongside `spot_scaled`.
pub fn strike_at_scale_zero(strike: u128, strike_scale: u8) -> f64 {
    strike as f64 / 10f64.powi(strike_scale as i32)
}

/// Returned sigma is the live realized vol when available, else `fallback`.
pub fn resolve_sigma(live_sigma: Option<f64>, fallback: f64) -> f64 {
    live_sigma.unwrap_or(fallback)
}

/// The full per-RFQ decision: compose the arithmetic, then either emit a
/// `Quote` or a `Decline` reason. Staleness/spot-error handling happens
/// upstream in the caller (which has the cache); this fn only needs the
/// resolved spot and sigma.
pub fn price_rfq(
    cfg: &PricingConfig,
    payload: &RfqBroadcastPayload,
    spot_scaled: u64,
    sigma: f64,
    now_ms: u64,
) -> PriceDecision {
    let t_years = time_to_expiry_years(payload.expiry_ms, now_ms);
    let strike_scaled = strike_at_scale_zero(payload.strike, payload.strike_scale);
    let inputs = CallInputs {
        spot: spot_scaled as f64,
        strike: strike_scaled,
        t_years,
        r: cfg.rate,
        sigma,
    };
    let per_unit = call_price_per_unit(inputs);
    let premium = premium_for_write(per_unit, payload.write_amount);
    if premium == 0 {
        return PriceDecision::Decline {
            reason: "priced to zero".into(),
        };
    }
    PriceDecision::Quote {
        premium,
        valid_until_ms: now_ms.saturating_add(cfg.quote_ttl_ms),
        spot_scaled,
        strike_scaled,
        t_years,
        sigma,
        per_unit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use protocol_types::ids::ObjectId;
    use protocol_types::sides::Side;
    use pyth_client::CachedPrice;

    fn rfq(expiry_ms: u64, strike: u128, strike_scale: u8, write_amount: u64) -> RfqBroadcastPayload {
        RfqBroadcastPayload {
            bucket_id: ObjectId::new([0u8; 32]),
            write_amount,
            side: Side::Trader,
            deadline_ms: expiry_ms,
            strike,
            strike_scale,
            expiry_ms,
        }
    }

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    // -- compute_spot_from_prices ---------------------------------------

    #[test]
    fn spot_btc_usdc_same_decimals() {
        // BTC = $60_000, USDC = $1.0, both 8d → spot = 60_000 * 10^0 = 60_000
        let s = compute_spot_from_prices(60_000.0, 1.0, 8, 8).unwrap();
        assert_eq!(s, 60_000);
    }

    #[test]
    fn spot_scales_by_decimal_delta() {
        // Underlying 8d, settlement 6d → 10^(6-8) = 0.01 → spot 60_000 * 0.01 = 600
        let s = compute_spot_from_prices(60_000.0, 1.0, 8, 6).unwrap();
        assert_eq!(s, 600);
        // Other way around: 10^(8-6) = 100 → spot 60_000 * 100 = 6_000_000
        let s = compute_spot_from_prices(60_000.0, 1.0, 6, 8).unwrap();
        assert_eq!(s, 6_000_000);
    }

    #[test]
    fn spot_non_dollar_settlement() {
        // Underlying $60_000, settlement $1500 (e.g. ETH-quoted): spot ≈ 40
        let s = compute_spot_from_prices(60_000.0, 1_500.0, 8, 8).unwrap();
        assert_eq!(s, 40);
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
        assert_eq!(spot, 60_000);
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

    // -- time_to_expiry_years -------------------------------------------

    #[test]
    fn t_years_simple_cases() {
        let year_ms = 1000 * 86_400 * 365u64;
        close(time_to_expiry_years(year_ms, 0), 1.0, 1e-12);
        close(time_to_expiry_years(0, 0), 0.0, 1e-12);
        // Expired bucket clamps to zero (no negative time).
        close(time_to_expiry_years(0, year_ms), 0.0, 1e-12);
        // 30 days ≈ 30/365
        let thirty_days_ms = 1000 * 86_400 * 30u64;
        close(time_to_expiry_years(thirty_days_ms, 0), 30.0 / 365.0, 1e-12);
    }

    // -- strike_at_scale_zero -------------------------------------------

    #[test]
    fn strike_scale_zero_is_identity() {
        close(strike_at_scale_zero(60_000, 0), 60_000.0, 1e-12);
    }

    #[test]
    fn strike_scale_divides() {
        // strike=60_000_000, scale=3 → 60_000
        close(strike_at_scale_zero(60_000_000, 3), 60_000.0, 1e-12);
        // scale=9, strike=1 → 1e-9
        close(strike_at_scale_zero(1, 9), 1e-9, 1e-18);
    }

    // -- resolve_sigma ---------------------------------------------------

    #[test]
    fn sigma_prefers_live() {
        assert_eq!(resolve_sigma(Some(0.42), 0.6), 0.42);
    }

    #[test]
    fn sigma_falls_back() {
        assert_eq!(resolve_sigma(None, 0.6), 0.6);
    }

    // -- price_rfq ------------------------------------------------------

    fn pricing_cfg() -> PricingConfig {
        PricingConfig {
            rate: 0.05,
            quote_ttl_ms: 30_000,
        }
    }

    #[test]
    fn price_rfq_quotes_atm_textbook_value() {
        // S=K=100, T=1y, r=5%, σ=20%, write=1 → premium ≈ floor(10.4506) = 10.
        let year_ms = 1000 * 86_400 * 365u64;
        let p = rfq(year_ms, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 100, 0.20, 0);
        match d {
            PriceDecision::Quote { premium, valid_until_ms, spot_scaled, strike_scaled, t_years, sigma, per_unit } => {
                assert_eq!(premium, 10);
                assert_eq!(valid_until_ms, 30_000);
                assert_eq!(spot_scaled, 100);
                close(strike_scaled, 100.0, 1e-12);
                close(t_years, 1.0, 1e-12);
                assert_eq!(sigma, 0.20);
                close(per_unit, 10.4506, 0.01);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_declines_when_priced_to_zero() {
        // Spot far below strike, no time → intrinsic = 0 → premium = 0.
        let p = rfq(0, 200, 0, 1_000_000);
        let d = price_rfq(&pricing_cfg(), &p, 100, 0.2, 0);
        match d {
            PriceDecision::Decline { reason } => assert_eq!(reason, "priced to zero"),
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_handles_expired_bucket_as_intrinsic() {
        // expiry_ms in the past, spot > strike — should price to intrinsic
        // and *not* decline.
        let p = rfq(0, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 150, 0.2, 1_000);
        match d {
            PriceDecision::Quote { premium, t_years, .. } => {
                assert_eq!(premium, 50); // intrinsic = 150 - 100, times write=1
                close(t_years, 0.0, 1e-12);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_strike_scale_is_applied() {
        // strike=100_000_000, scale=6 → effective strike = 100. With spot 110
        // and zero time, intrinsic = 10 per unit, write 7 → premium 70.
        let p = rfq(0, 100_000_000, 6, 7);
        let d = price_rfq(&pricing_cfg(), &p, 110, 0.2, 0);
        match d {
            PriceDecision::Quote { premium, strike_scaled, .. } => {
                close(strike_scaled, 100.0, 1e-12);
                assert_eq!(premium, 70);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_premium_scales_with_write_amount() {
        // Same option, double the size → double the premium (within floor).
        let year_ms = 1000 * 86_400 * 365u64;
        let p1 = rfq(year_ms, 100, 0, 100);
        let p2 = rfq(year_ms, 100, 0, 200);
        let d1 = price_rfq(&pricing_cfg(), &p1, 100, 0.20, 0);
        let d2 = price_rfq(&pricing_cfg(), &p2, 100, 0.20, 0);
        let (a, b) = match (&d1, &d2) {
            (PriceDecision::Quote { premium: a, .. }, PriceDecision::Quote { premium: b, .. }) => (*a, *b),
            _ => panic!("expected two Quotes"),
        };
        // Doubling write_amount roughly doubles premium; floor() drift is at
        // most 1 unit, so 2*a and b should be within a few of each other.
        assert!(b >= 2 * a - 2 && b <= 2 * a + 2, "a={a}, b={b}");
    }

    #[test]
    fn price_rfq_valid_until_uses_ttl() {
        let p = rfq(0, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 150, 0.2, 10_000);
        match d {
            PriceDecision::Quote { valid_until_ms, .. } => {
                assert_eq!(valid_until_ms, 40_000); // 10_000 + ttl 30_000
            }
            _ => panic!("expected Quote"),
        }
    }

    #[test]
    fn price_rfq_zero_vol_uses_discounted_intrinsic() {
        // σ=0: pricing collapses to discounted intrinsic. With S=110, K=100,
        // T=1, r=0 → premium = 10 * write.
        let year_ms = 1000 * 86_400 * 365u64;
        let p = rfq(year_ms, 100, 0, 1);
        let cfg = PricingConfig { rate: 0.0, quote_ttl_ms: 30_000 };
        let d = price_rfq(&cfg, &p, 110, 0.0, 0);
        match d {
            PriceDecision::Quote { premium, .. } => assert_eq!(premium, 10),
            _ => panic!("expected Quote"),
        }
    }
}
