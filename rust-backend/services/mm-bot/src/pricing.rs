//! Pure market-data chassis helpers shared by the desk's flows: spot
//! construction from Pyth prices, staleness/confidence checks, strike
//! rescaling, time-to-expiry, realized-vol selection with a fallback, and
//! the pair-match gate.
//!
//! The old vol-markup/smile quote model (`price_rfq` and friends) died in
//! the SO-299 strategy reset — quote pricing now lives in `desk::` on top
//! of the `crates/pricing` surface/american/desk modules.

use std::time::Duration;

use protocol_types::asset::canonicalize_move_type;
use pyth_client::{PriceCache, PriceFeedId};

/// Whether the bucket's pair (as resolved from api-service) is one this bot
/// sources a Pyth spot for. Both sides are canonicalized so a bare chain
/// `TypeName` matches a `0x`-padded configured type.
pub fn serves_pair(
    bucket_underlying: &str,
    bucket_settlement: &str,
    cfg_underlying: &str,
    cfg_settlement: &str,
) -> bool {
    canonicalize_move_type(bucket_underlying) == canonicalize_move_type(cfg_underlying)
        && canonicalize_move_type(bucket_settlement) == canonicalize_move_type(cfg_settlement)
}

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

/// Time to expiry in years, saturating at zero so an already-expired
/// bucket prices to intrinsic.
pub fn time_to_expiry_years(expiry_ms: u64, now_ms: u64) -> f64 {
    let ms = expiry_ms.saturating_sub(now_ms);
    ms as f64 / 1000.0 / 86_400.0 / 365.0
}

/// Rebase the bucket's `(strike, strike_scale)` pair onto scale=0
/// (settlement raw-units per underlying raw-unit) so it can be plugged
/// into the pricing model alongside a scale-0 spot.
///
/// Precision: the conversion goes through `f64`, which is exact for
/// integers up to 2^53 ≈ 9e15. Strikes whose raw `u128` magnitude
/// exceeds that lose precision past the 15th significant digit.
pub fn rebase_strike_to_scale_zero(strike: u128, strike_scale: u8) -> f64 {
    strike as f64 / 10f64.powi(strike_scale as i32)
}

/// A resolved sigma, flagged with whether it is the config fallback (cold
/// vol buffer) rather than the live estimate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SigmaEstimate {
    pub sigma: f64,
    pub is_fallback: bool,
}

/// Live realized vol when available, else `fallback` (flagged as such).
/// Two live windows feed in — a short one that tracks the current regime and
/// a long one that remembers it — and the max wins: one calm day must not
/// sell options below what the trailing week actually realized.
pub fn resolve_sigma(
    live_short: Option<f64>,
    live_long: Option<f64>,
    fallback: f64,
) -> SigmaEstimate {
    match (live_short, live_long) {
        (None, None) => SigmaEstimate { sigma: fallback, is_fallback: true },
        (short, long) => SigmaEstimate {
            sigma: short.unwrap_or(0.0).max(long.unwrap_or(0.0)),
            is_fallback: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use pyth_client::CachedPrice;

    // A representative configured pair for the `serves_pair` tests below.
    const UNDERLYING: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tbtc::TBTC";
    const SETTLEMENT: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tusdc::TUSDC";

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

    // -- rebase_strike_to_scale_zero ------------------------------------

    #[test]
    fn rebase_strike_scale_zero_is_identity() {
        close(rebase_strike_to_scale_zero(60_000, 0), 60_000.0, 1e-12);
    }

    #[test]
    fn rebase_strike_divides_by_ten_to_the_scale() {
        // strike=60_000_000, scale=3 → 60_000
        close(rebase_strike_to_scale_zero(60_000_000, 3), 60_000.0, 1e-12);
        // scale=9, strike=1 → 1e-9
        close(rebase_strike_to_scale_zero(1, 9), 1e-9, 1e-18);
    }

    #[test]
    fn rebase_strike_high_scale_round_trips_within_f64_precision() {
        // strike=100 * 10^18, scale=18 → exactly 100.0
        let s = rebase_strike_to_scale_zero(100_000_000_000_000_000_000u128, 18);
        close(s, 100.0, 1e-9);
        // Just past f64's integer-exact range (2^53 ≈ 9.007e15): the
        // conversion still works, but loses precision in the low digits.
        // We pin the magnitude is right (not the low bits).
        let almost_2pow53 = (1u128 << 53) + 1;
        let s = rebase_strike_to_scale_zero(almost_2pow53, 0);
        assert!((s - (1u128 << 53) as f64).abs() <= 1.0, "got {s}");
    }

    // -- resolve_sigma ---------------------------------------------------

    #[test]
    fn sigma_prefers_live() {
        assert_eq!(
            resolve_sigma(Some(0.42), None, 0.6),
            SigmaEstimate { sigma: 0.42, is_fallback: false }
        );
    }

    #[test]
    fn sigma_falls_back() {
        assert_eq!(
            resolve_sigma(None, None, 0.6),
            SigmaEstimate { sigma: 0.6, is_fallback: true }
        );
    }

    #[test]
    fn sigma_blends_windows_by_max() {
        // Calm day (short 0.3) after a wild week (long 0.9): quote the week.
        assert_eq!(
            resolve_sigma(Some(0.3), Some(0.9), 0.6),
            SigmaEstimate { sigma: 0.9, is_fallback: false }
        );
        // Wild day after a calm week: quote the day.
        assert_eq!(
            resolve_sigma(Some(0.9), Some(0.3), 0.6),
            SigmaEstimate { sigma: 0.9, is_fallback: false }
        );
        // One live window is enough to count as live.
        assert_eq!(
            resolve_sigma(None, Some(0.5), 0.6),
            SigmaEstimate { sigma: 0.5, is_fallback: false }
        );
    }

    // -- serves_pair (pair gate) ----------------------------------------

    #[test]
    fn serves_pair_rejects_foreign_underlying() {
        // A TBTC/TUSDC desk must NOT quote a TWAL bucket: the caller gates
        // on this before pricing.
        let twal = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::twal::TWAL";
        assert!(!serves_pair(twal, SETTLEMENT, UNDERLYING, SETTLEMENT));
    }

    #[test]
    fn serves_pair_matches_despite_noncanonical_type() {
        // api-service emits canonical types, but be robust: a bare chain
        // `TypeName` (no `0x` / padding) must still match the configured pair.
        let bare_under = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tbtc::TBTC";
        let bare_settle = "9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tusdc::TUSDC";
        assert!(serves_pair(bare_under, bare_settle, UNDERLYING, SETTLEMENT));
    }
}
