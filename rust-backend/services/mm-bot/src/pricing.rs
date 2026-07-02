//! Pure helpers that drive the bot's per-RFQ decision.
//!
//! These functions are deliberately decoupled from IO so the entire
//! market-making flow can be unit-tested: spot construction from Pyth
//! prices, staleness checks, strike rescaling, time-to-expiry, vol
//! selection with a fallback, Black-Scholes valuation, and the final
//! quote/decline decision.

use std::time::Duration;

use pricing::{
    call_price_per_unit, premium_for_write, premium_for_write_ceil, put_price_per_unit, CallInputs,
};
use protocol_types::asset::canonicalize_move_type;
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId};

/// Knobs that affect quote arithmetic but are independent of staleness/IO.
#[derive(Clone, Copy, Debug)]
pub struct PricingConfig {
    /// Annualized risk-free rate, continuous compounding.
    pub rate: f64,
    /// How long the quote we emit stays valid, in milliseconds.
    pub quote_ttl_ms: u64,
    /// Ask-side markup, in basis points, applied when we quote as the Writer
    /// MM (retail is buying — `Side::Trader`): the premium we charge is marked
    /// *up* off the Black-Scholes mid.
    pub ask_markup_bps: u64,
    /// Bid-side markdown, in basis points, applied when we quote as the Trader
    /// MM (retail is writing — `Side::Writer`): the premium we pay is marked
    /// *down* off the mid.
    pub bid_markdown_bps: u64,
}

/// The bucket-resolved + request inputs for pricing one RFQ. The bucket fields
/// (`strike`, `strike_scale`, `expiry_ms`) come from the api-service lookup —
/// never the wire broadcast — while `write_amount`/`side` are the request
/// parameters. Decoupled from the wire payload so pricing stays pure.
#[derive(Clone, Copy, Debug)]
pub struct RfqPricingInputs {
    /// Option size, in underlying smallest-units.
    pub write_amount: u64,
    /// Which side retail is on (drives the spread direction).
    pub side: Side,
    /// Bucket's on-chain strike; real ratio is `strike / 10^strike_scale`.
    pub strike: u128,
    /// 0..=9.
    pub strike_scale: u8,
    /// Bucket expiry as a Sui clock millisecond timestamp.
    pub expiry_ms: u64,
    /// `true` when the bucket is a cash-secured put: the Black-Scholes mid
    /// comes from [`pricing::put_price_per_unit`] instead of the call pricer.
    /// The spread / premium-scaling logic is identical to the call path.
    pub is_put: bool,
}

/// Whether the bucket's pair (as resolved from api-service) is the one this bot
/// sources a Pyth spot for. The bot reads a single `(underlying, settlement)`
/// pair, so any other bucket must be declined — pricing it against the wrong
/// spot yields a nonsense premium. Both sides are canonicalized so a bare chain
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

/// Apply the side-aware spread to the Black-Scholes mid per-unit price.
///
/// We serve both sides off one Account, so the spread is what makes the book
/// two-sided: we charge above mid on the ask (we're writing to a retail
/// trader) and pay below mid on the bid (we're buying from a retail writer).
fn apply_spread(per_unit_mid: f64, side: Side, cfg: &PricingConfig) -> f64 {
    let bps = match side {
        Side::Trader => cfg.ask_markup_bps as f64,
        Side::Writer => -(cfg.bid_markdown_bps as f64),
    };
    per_unit_mid * (1.0 + bps / 10_000.0)
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
        /// Settlement-raw per underlying-raw (may be sub-1 for cheap assets).
        spot_scaled: f64,
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
/// Equivalent to `(underlying_usd / settlement_usd) * 10^(settle_dec - under_dec)`.
///
/// Returned as `f64`, NOT rounded to an integer: for a sub-dollar underlying
/// (DEEP ≈ $0.016) against an equal-decimals settlement the ratio is well below
/// 1 (e.g. 0.016 settlement-raw per underlying-raw), so rounding to a u64 would
/// collapse it to 0 and price every such option to zero. Black-Scholes is
/// float-valued anyway, and `rebase_strike_to_scale_zero` keeps the strike at
/// the same `f64` scale, so the two stay comparable.
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
/// [`PriceCache`], applying staleness bounds.
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
/// into Black-Scholes alongside `spot_scaled`.
///
/// Why scale=0: `compute_spot` produces `spot_scaled` as an integer at
/// scale=0, and the BS call price is invariant to multiplying `S` and
/// `K` by a common factor (`d1` carries `ln(S/K)`, so the ratio is
/// what matters). Bringing both quantities to a common scale before
/// the math keeps the per-unit price in settlement-raw-per-underlying-
/// raw, which is exactly what `premium_for_write(per_unit, write_amount)`
/// needs.
///
/// Precision: the conversion goes through `f64`, which is exact for
/// integers up to 2^53 ≈ 9e15. Strikes whose raw `u128` magnitude
/// exceeds that lose precision past the 15th significant digit.
pub fn rebase_strike_to_scale_zero(strike: u128, strike_scale: u8) -> f64 {
    strike as f64 / 10f64.powi(strike_scale as i32)
}

/// Returned sigma is the live realized vol when available, else `fallback`.
pub fn resolve_sigma(live_sigma: Option<f64>, fallback: f64) -> f64 {
    live_sigma.unwrap_or(fallback)
}

/// The full per-RFQ decision: compose the arithmetic, then either emit a
/// `Quote` or a `Decline` reason. Staleness/spot-error handling and the
/// pair-match gate (see [`serves_pair`]) happen upstream in the caller; this fn
/// receives the already-resolved spot, sigma, and bucket inputs.
pub fn price_rfq(
    cfg: &PricingConfig,
    inputs: &RfqPricingInputs,
    spot_scaled: f64,
    sigma: f64,
    now_ms: u64,
) -> PriceDecision {
    let t_years = time_to_expiry_years(inputs.expiry_ms, now_ms);
    let strike_scaled = rebase_strike_to_scale_zero(inputs.strike, inputs.strike_scale);
    // PutInputs == CallInputs; the only difference is which BS leg we evaluate.
    let bs_inputs = CallInputs {
        spot: spot_scaled,
        strike: strike_scaled,
        t_years,
        r: cfg.rate,
        sigma,
    };
    let per_unit_mid = if inputs.is_put {
        // The on-chain puts are American (put_bucket allows exercise any time
        // pre-expiry), so the quote can never sit below intrinsic: the
        // European value dips under K − S when r > 0, and an ask below
        // intrinsic is bought + exercised immediately for a riskless profit.
        put_price_per_unit(bs_inputs).max((strike_scaled - spot_scaled).max(0.0))
    } else {
        call_price_per_unit(bs_inputs)
    };
    let per_unit = apply_spread(per_unit_mid, inputs.side, cfg);
    // Ask rounds up, bid rounds down: flooring the ask would undercharge by
    // up to one settlement raw-unit per quote.
    let premium = match inputs.side {
        Side::Trader => premium_for_write_ceil(per_unit, inputs.write_amount),
        Side::Writer => premium_for_write(per_unit, inputs.write_amount),
    };
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

    use protocol_types::sides::Side;
    use pyth_client::CachedPrice;

    // A representative configured pair for the `serves_pair` tests below.
    const UNDERLYING: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tbtc::TBTC";
    const SETTLEMENT: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tusdc::TUSDC";

    fn rfq(expiry_ms: u64, strike: u128, strike_scale: u8, write_amount: u64) -> RfqPricingInputs {
        rfq_side(Side::Trader, expiry_ms, strike, strike_scale, write_amount)
    }

    fn rfq_side(
        side: Side,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
        write_amount: u64,
    ) -> RfqPricingInputs {
        RfqPricingInputs {
            write_amount,
            side,
            strike,
            strike_scale,
            expiry_ms,
            is_put: false,
        }
    }

    fn put_rfq(
        side: Side,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
        write_amount: u64,
    ) -> RfqPricingInputs {
        RfqPricingInputs {
            write_amount,
            side,
            strike,
            strike_scale,
            expiry_ms,
            is_put: true,
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
            ask_markup_bps: 0,
            bid_markdown_bps: 0,
        }
    }

    #[test]
    fn price_rfq_quotes_atm_textbook_value() {
        // S=K=100, T=1y, r=5%, σ=20%, write=1 → BS ≈ 10.4506; the default
        // `rfq` is trader-side (our ask), which rounds UP → 11.
        let year_ms = 1000 * 86_400 * 365u64;
        let p = rfq(year_ms, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 100.0, 0.20, 0);
        match d {
            PriceDecision::Quote { premium, valid_until_ms, spot_scaled, strike_scaled, t_years, sigma, per_unit } => {
                assert_eq!(premium, 11);
                assert_eq!(valid_until_ms, 30_000);
                close(spot_scaled, 100.0, 1e-12);
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
        let d = price_rfq(&pricing_cfg(), &p, 100.0, 0.2, 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 150.0, 0.2, 1_000);
        match d {
            PriceDecision::Quote { premium, t_years, .. } => {
                assert_eq!(premium, 50); // intrinsic = 150 - 100, times write=1
                close(t_years, 0.0, 1e-12);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_high_strike_scale_matches_scale_zero_hand_calc() {
        // Same option expressed two ways:
        //   (a) strike=100, scale=0 → effective 100.0
        //   (b) strike=100 * 10^18, scale=18 → effective 100.0
        // BS is invariant to common scaling of S/K, so the per-unit price
        // (and therefore the premium for the same write_amount) must match
        // bit-for-bit at scales the f64 conversion is exact for.
        let year_ms = 1000 * 86_400 * 365u64;
        let p_low = rfq(year_ms, 100, 0, 1_000_000);
        let p_high = rfq(year_ms, 100_000_000_000_000_000_000u128, 18, 1_000_000);
        let cfg = pricing_cfg();

        let d_low = price_rfq(&cfg, &p_low, 100.0, 0.20, 0);
        let d_high = price_rfq(&cfg, &p_high, 100.0, 0.20, 0);

        let (low, high) = match (&d_low, &d_high) {
            (
                PriceDecision::Quote { premium: a, strike_scaled: sa, per_unit: ua, .. },
                PriceDecision::Quote { premium: b, strike_scaled: sb, per_unit: ub, .. },
            ) => ((a, sa, ua), (b, sb, ub)),
            _ => panic!("expected two Quotes, got {d_low:?} / {d_high:?}"),
        };
        // Effective strike and per-unit price must be identical.
        close(*low.1, *high.1, 1e-9);
        close(*low.2, *high.2, 1e-12);
        // Premium rounds per_unit * write the same way, so equal per_unit
        // ⇒ equal premium.
        assert_eq!(low.0, high.0);

        // Sanity-check the magnitude against a textbook ATM (S=K=100,
        // T=1y, r=5%, σ=20% → ~10.4506). With write=1M, premium ≈ 10.45M.
        assert!((10_000_000..=11_000_000).contains(low.0), "premium {} off textbook", low.0);
    }

    #[test]
    fn price_rfq_strike_scale_is_applied() {
        // strike=100_000_000, scale=6 → effective strike = 100. With spot 110
        // and zero time, intrinsic = 10 per unit, write 7 → premium 70.
        let p = rfq(0, 100_000_000, 6, 7);
        let d = price_rfq(&pricing_cfg(), &p, 110.0, 0.2, 0);
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
        let d1 = price_rfq(&pricing_cfg(), &p1, 100.0, 0.20, 0);
        let d2 = price_rfq(&pricing_cfg(), &p2, 100.0, 0.20, 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 150.0, 0.2, 10_000);
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
        let cfg = PricingConfig {
            rate: 0.0,
            quote_ttl_ms: 30_000,
            ask_markup_bps: 0,
            bid_markdown_bps: 0,
        };
        let d = price_rfq(&cfg, &p, 110.0, 0.0, 0);
        match d {
            PriceDecision::Quote { premium, .. } => assert_eq!(premium, 10),
            _ => panic!("expected Quote"),
        }
    }

    // -- price_rfq put path ---------------------------------------------

    #[test]
    fn price_rfq_put_quotes_atm_textbook_value() {
        // S=K=100, T=1y, r=5%, σ=20% → BS put ≈ 5.5735; trader-side ask
        // rounds UP → 6.
        let year_ms = 1000 * 86_400 * 365u64;
        let p = put_rfq(Side::Trader, year_ms, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 100.0, 0.20, 0);
        match d {
            PriceDecision::Quote { premium, per_unit, .. } => {
                assert_eq!(premium, 6);
                close(per_unit, 5.5735, 0.01);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_put_never_quotes_below_intrinsic() {
        // Deep-ITM put with r > 0: the European value K·e^(−rτ)·N(−d2) −
        // S·N(−d1) ≈ 35.2 sits BELOW the 40.0 intrinsic. The puts are
        // American-exercisable, so the quote must floor at intrinsic on both
        // sides — an ask below it is free money for the counterparty.
        let year_ms = 1000 * 86_400 * 365u64;
        let ask = premium_of(&price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            60.0,
            0.20,
            0,
        ));
        let bid = premium_of(&price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Writer, year_ms, 100, 0, 1_000_000),
            60.0,
            0.20,
            0,
        ));
        let intrinsic_total = 40 * 1_000_000u64;
        assert!(ask >= intrinsic_total, "ask {ask} below intrinsic {intrinsic_total}");
        assert!(bid >= intrinsic_total, "bid {bid} below intrinsic {intrinsic_total}");
    }

    #[test]
    fn price_rfq_put_differs_from_call_otm() {
        // Spot well above strike: the call is deep ITM, the put deep OTM, so
        // the same inputs must price very differently across the two legs.
        let year_ms = 1000 * 86_400 * 365u64;
        let call = price_rfq(&pricing_cfg(), &rfq(year_ms, 100, 0, 1_000_000), 150.0, 0.20, 0);
        let put = price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            150.0,
            0.20,
            0,
        );
        assert!(premium_of(&call) > premium_of(&put), "{call:?} vs {put:?}");
    }

    #[test]
    fn price_rfq_put_expired_prices_to_intrinsic() {
        // Expired put, spot below strike → intrinsic = K - S per unit.
        let p = put_rfq(Side::Trader, 0, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 60.0, 0.2, 1_000);
        match d {
            PriceDecision::Quote { premium, t_years, .. } => {
                assert_eq!(premium, 40); // 100 - 60
                close(t_years, 0.0, 1e-12);
            }
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn price_rfq_put_spread_marks_ask_up_and_bid_down() {
        // Same ATM put priced trader-side (ask) and writer-side (bid) with a
        // 100/200 bps spread: ask above the put mid, bid below.
        let year_ms = 1000 * 86_400 * 365u64;
        let cfg = PricingConfig {
            rate: 0.05,
            quote_ttl_ms: 30_000,
            ask_markup_bps: 100,
            bid_markdown_bps: 200,
        };
        let mid = premium_of(&price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        let ask = premium_of(&price_rfq(
            &cfg,
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        let bid = premium_of(&price_rfq(
            &cfg,
            &put_rfq(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        assert!(ask > mid, "ask {ask} should exceed mid {mid}");
        assert!(bid < mid, "bid {bid} should be below mid {mid}");
    }

    // -- serves_pair (pair gate) ----------------------------------------

    #[test]
    fn serves_pair_rejects_foreign_underlying() {
        // The bug: a TBTC/TUSDC bot must NOT quote a TWAL bucket. The caller
        // gates on this before pricing, so the looked-up TWAL bucket is
        // declined instead of priced against the TBTC spot (~$313k for 0.5 TWAL).
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

    // -- spread ---------------------------------------------------------

    fn premium_of(d: &PriceDecision) -> u64 {
        match d {
            PriceDecision::Quote { premium, .. } => *premium,
            other => panic!("expected Quote, got {other:?}"),
        }
    }

    #[test]
    fn spread_marks_ask_up_and_bid_down_around_mid() {
        // Same ATM option (S=K=100, T=1y, σ=20%, write=1M → mid ≈ 10.45M)
        // priced as a trader-side ask and a writer-side bid with a 100bps /
        // 200bps spread. Ask must sit above mid, bid below.
        let year_ms = 1000 * 86_400 * 365u64;
        let cfg = PricingConfig {
            rate: 0.05,
            quote_ttl_ms: 30_000,
            ask_markup_bps: 100,
            bid_markdown_bps: 200,
        };
        let mid = premium_of(&price_rfq(&pricing_cfg(), &rfq(year_ms, 100, 0, 1_000_000), 100.0, 0.20, 0));
        let ask = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        let bid = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        assert!(ask > mid, "ask {ask} should exceed mid {mid}");
        assert!(bid < mid, "bid {bid} should be below mid {mid}");
        // Markup/markdown are proportional to the configured bps.
        close(ask as f64 / mid as f64, 1.01, 1e-3);
        close(bid as f64 / mid as f64, 0.98, 1e-3);
    }

    #[test]
    fn zero_spread_is_side_independent_and_matches_mid() {
        // With both bps at zero, trader and writer sides price the same bare
        // Black-Scholes mid; the only residual difference is the rounding
        // direction (ask ceils, bid floors), at most one raw unit.
        let year_ms = 1000 * 86_400 * 365u64;
        let ask = premium_of(&price_rfq(
            &pricing_cfg(),
            &rfq_side(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        let bid = premium_of(&price_rfq(
            &pricing_cfg(),
            &rfq_side(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            0.20,
            0,
        ));
        assert!(ask >= bid && ask - bid <= 1, "ask {ask}, bid {bid}");
    }
}
