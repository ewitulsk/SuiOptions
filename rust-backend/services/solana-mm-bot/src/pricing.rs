//! Pure helpers that drive the bot's per-RFQ decision — the verbatim port
//! of the Sui mm-bot's `pricing.rs`.
//!
//! These functions are deliberately decoupled from IO so the entire
//! market-making flow can be unit-tested: spot construction from Pyth
//! prices, staleness checks, strike rescaling, time-to-expiry, vol
//! selection with a fallback, Black-Scholes valuation, and the final
//! quote/decline decision.
//!
//! The only Solana delta is [`serves_pair`]: token identity is the SPL
//! mint address (base58), compared byte-exact — there is no
//! `canonicalize_move_type` equivalent to apply.

use std::time::Duration;

pub use pricing::smile::Smile;
use pricing::{
    call_delta, call_price_per_unit, premium_for_write, premium_for_write_ceil, put_delta,
    put_price_per_unit, CallInputs,
};
use protocol_types::sides::Side;
use pyth_client::{PriceCache, PriceFeedId};

/// Knobs that affect quote arithmetic but are independent of staleness/IO.
#[derive(Clone, Copy, Debug)]
pub struct PricingConfig {
    /// Annualized risk-free rate, continuous compounding.
    pub rate: f64,
    /// How long the quote we emit stays valid, in milliseconds.
    pub quote_ttl_ms: u64,
    /// Ask-side *minimum* markup, in basis points of premium, applied when we
    /// quote as the Writer MM (retail is buying — `Side::Trader`). The
    /// vol-space spread below usually dominates; this floor is what's left
    /// deep ITM, where vega ≈ 0 and the vol spread collapses onto the mid.
    pub ask_markup_bps: u64,
    /// Bid-side *minimum* markdown, in basis points of premium, applied when
    /// we quote as the Trader MM (retail is writing — `Side::Writer`).
    pub bid_markdown_bps: u64,
    /// Multiplier (≥ 1) on sigma for the ask leg: we sell options at
    /// marked-up vol. Spreading in vol space scales the spread with the vega
    /// actually being warehoused — a flat premium-bps spread is dust far OTM
    /// and pure intrinsic markup deep ITM. 1.0 disables.
    pub ask_vol_markup: f64,
    /// Multiplier (≤ 1) on sigma for the bid leg: we buy options at
    /// marked-down vol. 1.0 disables.
    pub bid_vol_markdown: f64,
    /// Last-look charge: a signed quote is a free option on the market
    /// moving over its TTL — the counterparty executes only when the move
    /// favors them. We charge `mult · |delta| · spot · sigma · √(ttl_years)`
    /// (the expected favorable move, delta-scaled) on the ask and shade it
    /// off the bid. 0.0 disables.
    pub ttl_charge_mult: f64,
    /// Extra vol widening (≥ 1) applied while sigma is the config fallback
    /// (cold vol buffer): the ask leg's sigma is multiplied and the bid
    /// leg's divided by this, so quoting blind is quoted wide. 1.0 disables.
    pub fallback_vol_penalty: f64,
    /// Strike-dependent vol ([`pricing::smile::Smile`]): the base sigma is
    /// evaluated through the smile at the bucket's strike before spreads.
    /// Default (flat) preserves single-sigma pricing across the whole grid;
    /// per-symbol overrides come from the bot's `[smiles]` config table.
    pub smile: Smile,
    /// Decline any RFQ whose notional (spot × write_amount, in settlement
    /// smallest-units) exceeds this — an unbounded quote is an unbounded
    /// vega/delta position from one signature. 0 disables.
    pub max_quote_notional: u64,
    /// Size-dependent widening: extra proportional vol widening per
    /// `size_ref_notional` of quote notional (ask sigma ×, bid sigma ÷).
    /// Premium impact is ≈ vega-proportional, so big clips pay for the
    /// inventory they move. 0.0 disables.
    pub size_widening_vol: f64,
    /// Reference notional (settlement smallest-units) at which
    /// `size_widening_vol` applies in full.
    pub size_ref_notional: u64,
}

/// The bucket-resolved + request inputs for pricing one RFQ. The bucket fields
/// (`strike`, `strike_scale`, `expiry_ms`) come from the solana-api-service
/// lookup — never the wire broadcast — while `write_amount`/`side` are the
/// request parameters. Decoupled from the wire payload so pricing stays pure.
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
    /// Bucket expiry as a Unix millisecond timestamp.
    pub expiry_ms: u64,
    /// `true` when the bucket is a cash-secured put: the Black-Scholes mid
    /// comes from [`pricing::put_price_per_unit`] instead of the call pricer.
    /// The spread / premium-scaling logic is identical to the call path.
    pub is_put: bool,
}

/// Whether the bucket's pair (as resolved from solana-api-service) is one
/// this bot sources a Pyth spot for. Mints are base58 pubkey strings and
/// compare byte-exact — any other bucket must be declined, since pricing
/// it against the wrong spot yields a nonsense premium.
pub fn serves_pair(
    bucket_underlying: &str,
    bucket_settlement: &str,
    cfg_underlying: &str,
    cfg_settlement: &str,
) -> bool {
    bucket_underlying == cfg_underlying && bucket_settlement == cfg_settlement
}

/// Apply the side-aware spread to the Black-Scholes mid per-unit price.
///
/// We serve both sides off one MmAccount, so the spread is what makes the
/// book two-sided: we charge above mid on the ask (we're writing to a retail
/// trader) and pay below mid on the bid (we're buying from a retail writer).
fn apply_spread(per_unit_mid: f64, side: Side, cfg: &PricingConfig) -> f64 {
    let bps = match side {
        Side::Trader => cfg.ask_markup_bps as f64,
        Side::Writer => -(cfg.bid_markdown_bps as f64),
    };
    per_unit_mid * (1.0 + bps / 10_000.0)
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
/// against an equal-decimals settlement the ratio is well below 1, so rounding
/// to a u64 would collapse it to 0 and price every such option to zero.
/// Black-Scholes is float-valued anyway, and `rebase_strike_to_scale_zero`
/// keeps the strike at the same `f64` scale, so the two stay comparable.
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
/// into Black-Scholes alongside `spot_scaled`.
///
/// Why scale=0: `compute_spot` produces `spot_scaled` at scale=0, and the
/// BS call price is invariant to multiplying `S` and `K` by a common factor
/// (`d1` carries `ln(S/K)`, so the ratio is what matters). Bringing both
/// quantities to a common scale before the math keeps the per-unit price
/// in settlement-raw-per-underlying-raw, which is exactly what
/// `premium_for_write(per_unit, write_amount)` needs.
///
/// Precision: the conversion goes through `f64`, which is exact for
/// integers up to 2^53 ≈ 9e15. Strikes whose raw `u128` magnitude
/// exceeds that lose precision past the 15th significant digit.
pub fn rebase_strike_to_scale_zero(strike: u128, strike_scale: u8) -> f64 {
    strike as f64 / 10f64.powi(strike_scale as i32)
}

/// A resolved sigma, flagged with whether it is the config fallback (cold
/// vol buffer) rather than the live estimate — pricing widens the spread
/// while quoting blind (`PricingConfig::fallback_vol_penalty`).
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

const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;

/// Black-Scholes per-unit value at the given inputs; puts floor at intrinsic
/// because the on-chain puts are American (put_bucket allows exercise any
/// time pre-expiry): the European value dips under K − S when r > 0, and an
/// ask below intrinsic is bought + exercised immediately for a riskless
/// profit.
fn per_unit_at(bs: CallInputs, is_put: bool) -> f64 {
    if is_put {
        put_price_per_unit(bs).max((bs.strike - bs.spot).max(0.0))
    } else {
        call_price_per_unit(bs)
    }
}

/// The full per-RFQ decision: compose the arithmetic, then either emit a
/// `Quote` or a `Decline` reason. Staleness/spot-error handling and the
/// pair-match gate (see [`serves_pair`]) happen upstream in the caller; this fn
/// receives the already-resolved spot, sigma, and bucket inputs.
pub fn price_rfq(
    cfg: &PricingConfig,
    inputs: &RfqPricingInputs,
    spot_scaled: f64,
    sigma_est: SigmaEstimate,
    now_ms: u64,
) -> PriceDecision {
    let t_years = time_to_expiry_years(inputs.expiry_ms, now_ms);
    let strike_scaled = rebase_strike_to_scale_zero(inputs.strike, inputs.strike_scale);
    let SigmaEstimate { sigma, is_fallback } = sigma_est;
    // Size gate: one signature must not move unbounded notional.
    let notional = spot_scaled * inputs.write_amount as f64;
    if cfg.max_quote_notional > 0 && notional > cfg.max_quote_notional as f64 {
        return PriceDecision::Decline {
            reason: "size exceeds max quote notional".into(),
        };
    }
    // Strike-dependent vol: evaluate the configured smile at this strike.
    // Flat (default) leaves sigma untouched.
    let sigma = cfg.smile.sigma_at(sigma, spot_scaled, strike_scaled, t_years);
    // Vol-space spread: the ask leg prices at marked-up sigma, the bid leg at
    // marked-down sigma. Quoting on the fallback sigma widens further, and
    // size widening scales with the clip's notional (≈ vega-proportional
    // premium impact).
    let penalty = if is_fallback { cfg.fallback_vol_penalty.max(1.0) } else { 1.0 };
    let size_mult = if cfg.size_ref_notional > 0 && cfg.size_widening_vol > 0.0 {
        1.0 + cfg.size_widening_vol * (notional / cfg.size_ref_notional as f64)
    } else {
        1.0
    };
    let widen = penalty * size_mult;
    let sigma_side = match inputs.side {
        Side::Trader => sigma * cfg.ask_vol_markup.max(1.0) * widen,
        Side::Writer => sigma * cfg.bid_vol_markdown.clamp(0.0, 1.0) / widen,
    };
    // PutInputs == CallInputs; the only difference is which BS leg we evaluate.
    let bs_mid = CallInputs {
        spot: spot_scaled,
        strike: strike_scaled,
        t_years,
        r: cfg.rate,
        sigma,
    };
    let per_unit_mid = per_unit_at(bs_mid, inputs.is_put);
    let per_unit_vol = per_unit_at(CallInputs { sigma: sigma_side, ..bs_mid }, inputs.is_put);
    // The premium-bps spread is the minimum: it is all that separates the
    // sides deep ITM, where vega ≈ 0 and the vol spread collapses onto the
    // mid. Ask takes the larger of the two, bid the smaller.
    let per_unit_bps = apply_spread(per_unit_mid, inputs.side, cfg);
    let base = match inputs.side {
        Side::Trader => per_unit_vol.max(per_unit_bps),
        Side::Writer => per_unit_vol.min(per_unit_bps),
    };
    // Last-look charge (see `PricingConfig::ttl_charge_mult`): expected
    // favorable move over the quote's TTL, delta-scaled.
    let ttl_years = cfg.quote_ttl_ms as f64 / MS_PER_YEAR;
    let delta = if inputs.is_put { put_delta(bs_mid) } else { call_delta(bs_mid) };
    let ttl_charge =
        cfg.ttl_charge_mult.max(0.0) * delta.abs() * spot_scaled * sigma * ttl_years.sqrt();
    let per_unit = match inputs.side {
        Side::Trader => base + ttl_charge,
        Side::Writer => (base - ttl_charge).max(0.0),
    };
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

    // A representative configured pair for the `serves_pair` tests below —
    // SPL mint addresses, byte-exact.
    const UNDERLYING: &str = "So11111111111111111111111111111111111111112";
    const SETTLEMENT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

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

    /// A live (non-fallback) sigma, the default for pricing tests.
    fn live(sigma: f64) -> SigmaEstimate {
        SigmaEstimate { sigma, is_fallback: false }
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
        // Regression: a ~$0.0158 underlying against a $1 settlement, both 6d
        // → ratio 0.0158 settlement-raw per underlying-raw. A u64 round
        // would collapse this to 0 (every such option priced to zero); f64
        // keeps it.
        let s = compute_spot_from_prices(0.0158, 1.0, 6, 6).unwrap();
        close(s, 0.0158, 1e-12);
        // ~$0.0327, 9d underlying vs 6d settlement → 0.0327 * 10^-3.
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

    // -- price_rfq ------------------------------------------------------

    fn pricing_cfg() -> PricingConfig {
        PricingConfig {
            rate: 0.05,
            quote_ttl_ms: 30_000,
            ask_markup_bps: 0,
            bid_markdown_bps: 0,
            ask_vol_markup: 1.0,
            bid_vol_markdown: 1.0,
            ttl_charge_mult: 0.0,
            fallback_vol_penalty: 1.0,
            smile: Smile::default(),
            max_quote_notional: 0,
            size_widening_vol: 0.0,
            size_ref_notional: 0,
        }
    }

    #[test]
    fn price_rfq_quotes_atm_textbook_value() {
        // S=K=100, T=1y, r=5%, σ=20%, write=1 → BS ≈ 10.4506; the default
        // `rfq` is trader-side (our ask), which rounds UP → 11.
        let year_ms = 1000 * 86_400 * 365u64;
        let p = rfq(year_ms, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 100.0, live(0.20), 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 100.0, live(0.2), 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 150.0, live(0.2), 1_000);
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

        let d_low = price_rfq(&cfg, &p_low, 100.0, live(0.20), 0);
        let d_high = price_rfq(&cfg, &p_high, 100.0, live(0.20), 0);

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
        let d = price_rfq(&pricing_cfg(), &p, 110.0, live(0.2), 0);
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
        let d1 = price_rfq(&pricing_cfg(), &p1, 100.0, live(0.20), 0);
        let d2 = price_rfq(&pricing_cfg(), &p2, 100.0, live(0.20), 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 150.0, live(0.2), 10_000);
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
        let cfg = PricingConfig { rate: 0.0, ..pricing_cfg() };
        let d = price_rfq(&cfg, &p, 110.0, live(0.0), 0);
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
        let d = price_rfq(&pricing_cfg(), &p, 100.0, live(0.20), 0);
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
            live(0.20),
            0,
        ));
        let bid = premium_of(&price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Writer, year_ms, 100, 0, 1_000_000),
            60.0,
            live(0.20),
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
        let call = price_rfq(&pricing_cfg(), &rfq(year_ms, 100, 0, 1_000_000), 150.0, live(0.20), 0);
        let put = price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            150.0,
            live(0.20),
            0,
        );
        assert!(premium_of(&call) > premium_of(&put), "{call:?} vs {put:?}");
    }

    #[test]
    fn price_rfq_put_expired_prices_to_intrinsic() {
        // Expired put, spot below strike → intrinsic = K - S per unit.
        let p = put_rfq(Side::Trader, 0, 100, 0, 1);
        let d = price_rfq(&pricing_cfg(), &p, 60.0, live(0.2), 1_000);
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
            ask_markup_bps: 100,
            bid_markdown_bps: 200,
            ..pricing_cfg()
        };
        let mid = premium_of(&price_rfq(
            &pricing_cfg(),
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
            0,
        ));
        let ask = premium_of(&price_rfq(
            &cfg,
            &put_rfq(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
            0,
        ));
        let bid = premium_of(&price_rfq(
            &cfg,
            &put_rfq(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
            0,
        ));
        assert!(ask > mid, "ask {ask} should exceed mid {mid}");
        assert!(bid < mid, "bid {bid} should be below mid {mid}");
    }

    // -- serves_pair (pair gate) ----------------------------------------

    #[test]
    fn serves_pair_rejects_foreign_underlying() {
        // A TBTC/TUSDC bot must NOT quote a bucket in another underlying.
        // The caller gates on this before pricing, so the looked-up bucket
        // is declined instead of priced against the wrong spot.
        let other = "9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin";
        assert!(!serves_pair(other, SETTLEMENT, UNDERLYING, SETTLEMENT));
        assert!(!serves_pair(UNDERLYING, other, UNDERLYING, SETTLEMENT));
    }

    #[test]
    fn serves_pair_is_byte_exact_on_base58() {
        assert!(serves_pair(UNDERLYING, SETTLEMENT, UNDERLYING, SETTLEMENT));
        // No normalization on base58: a case-tweaked near-miss must not match.
        let near_miss = "so11111111111111111111111111111111111111112";
        assert!(!serves_pair(near_miss, SETTLEMENT, UNDERLYING, SETTLEMENT));
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
            ask_markup_bps: 100,
            bid_markdown_bps: 200,
            ..pricing_cfg()
        };
        let mid = premium_of(&price_rfq(&pricing_cfg(), &rfq(year_ms, 100, 0, 1_000_000), 100.0, live(0.20), 0));
        let ask = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Trader, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
            0,
        ));
        let bid = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
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
            live(0.20),
            0,
        ));
        let bid = premium_of(&price_rfq(
            &pricing_cfg(),
            &rfq_side(Side::Writer, year_ms, 100, 0, 1_000_000),
            100.0,
            live(0.20),
            0,
        ));
        assert!(ask >= bid && ask - bid <= 1, "ask {ask}, bid {bid}");
    }

    // -- vol-space spread, TTL charge, fallback penalty -------------------

    /// ATM weekly option sized so vega is meaningful; used by the spread tests.
    fn weekly_atm() -> (RfqPricingInputs, RfqPricingInputs, f64) {
        let week_ms = 1000 * 86_400 * 7u64;
        (
            rfq_side(Side::Trader, week_ms, 100, 0, 1_000_000),
            rfq_side(Side::Writer, week_ms, 100, 0, 1_000_000),
            100.0,
        )
    }

    #[test]
    fn vol_markup_widens_ask_and_markdown_widens_bid() {
        let (ask_rfq, bid_rfq, spot) = weekly_atm();
        let flat = pricing_cfg();
        let vol_spread = PricingConfig {
            ask_vol_markup: 1.10,
            bid_vol_markdown: 0.90,
            ..pricing_cfg()
        };
        let ask_flat = premium_of(&price_rfq(&flat, &ask_rfq, spot, live(0.60), 0));
        let bid_flat = premium_of(&price_rfq(&flat, &bid_rfq, spot, live(0.60), 0));
        let ask_vol = premium_of(&price_rfq(&vol_spread, &ask_rfq, spot, live(0.60), 0));
        let bid_vol = premium_of(&price_rfq(&vol_spread, &bid_rfq, spot, live(0.60), 0));
        assert!(ask_vol > ask_flat, "ask {ask_vol} !> {ask_flat}");
        assert!(bid_vol < bid_flat, "bid {bid_vol} !< {bid_flat}");
        // ATM the premium is ≈ linear in sigma, so a 10% vol markup moves the
        // ask by ≈ 10% — far more than a 100bps premium floor would.
        assert!(ask_vol as f64 > ask_flat as f64 * 1.05, "{ask_vol} vs {ask_flat}");
    }

    #[test]
    fn bps_floor_survives_deep_itm_where_vol_spread_collapses() {
        // Deep ITM call: premium ≈ intrinsic, vega ≈ 0, so the vol spread
        // alone would quote both sides at ≈ mid. The bps floor must keep the
        // book two-sided.
        let year_ms = 1000 * 86_400 * 365u64;
        let cfg = PricingConfig {
            ask_markup_bps: 100,
            bid_markdown_bps: 100,
            ask_vol_markup: 1.10,
            bid_vol_markdown: 0.90,
            ..pricing_cfg()
        };
        let ask = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Trader, year_ms, 10, 0, 1_000_000),
            100.0,
            live(0.05),
            0,
        ));
        let bid = premium_of(&price_rfq(
            &cfg,
            &rfq_side(Side::Writer, year_ms, 10, 0, 1_000_000),
            100.0,
            live(0.05),
            0,
        ));
        let mid = premium_of(&price_rfq(
            &pricing_cfg(),
            &rfq_side(Side::Trader, year_ms, 10, 0, 1_000_000),
            100.0,
            live(0.05),
            0,
        ));
        // Ask at least the 100bps floor above mid; bid at least 100bps below.
        assert!(ask as f64 >= mid as f64 * 1.0099, "ask {ask} vs mid {mid}");
        assert!(bid as f64 <= mid as f64 * 0.9901, "bid {bid} vs mid {mid}");
    }

    #[test]
    fn ttl_charge_widens_both_sides_monotonically() {
        let (ask_rfq, bid_rfq, spot) = weekly_atm();
        let no_charge = pricing_cfg();
        let charged = PricingConfig { ttl_charge_mult: 1.0, ..pricing_cfg() };
        let more_charged = PricingConfig { ttl_charge_mult: 2.0, ..pricing_cfg() };
        let ask0 = premium_of(&price_rfq(&no_charge, &ask_rfq, spot, live(0.60), 0));
        let ask1 = premium_of(&price_rfq(&charged, &ask_rfq, spot, live(0.60), 0));
        let ask2 = premium_of(&price_rfq(&more_charged, &ask_rfq, spot, live(0.60), 0));
        assert!(ask0 < ask1 && ask1 < ask2, "{ask0} {ask1} {ask2}");
        let bid0 = premium_of(&price_rfq(&no_charge, &bid_rfq, spot, live(0.60), 0));
        let bid1 = premium_of(&price_rfq(&charged, &bid_rfq, spot, live(0.60), 0));
        assert!(bid1 < bid0, "{bid1} !< {bid0}");
        // Sanity on magnitude: |delta|·S·σ·√ttl at 30s TTL, σ=0.6, ATM
        // delta ≈ 0.5 → ≈ 100·0.5·0.6·√(30/31.5M) ≈ 0.029 per unit ≈ 29k
        // on 1M written. The charge must be in that ballpark, not 100x off.
        let widened = ask1 - ask0;
        assert!((10_000..=60_000).contains(&widened), "ttl charge {widened}");
    }

    #[test]
    fn fallback_sigma_quotes_wider_than_live() {
        let (ask_rfq, bid_rfq, spot) = weekly_atm();
        let cfg = PricingConfig { fallback_vol_penalty: 1.5, ..pricing_cfg() };
        let fallback = SigmaEstimate { sigma: 0.60, is_fallback: true };
        let ask_live = premium_of(&price_rfq(&cfg, &ask_rfq, spot, live(0.60), 0));
        let ask_blind = premium_of(&price_rfq(&cfg, &ask_rfq, spot, fallback, 0));
        let bid_live = premium_of(&price_rfq(&cfg, &bid_rfq, spot, live(0.60), 0));
        let bid_blind = premium_of(&price_rfq(&cfg, &bid_rfq, spot, fallback, 0));
        assert!(ask_blind > ask_live, "{ask_blind} !> {ask_live}");
        assert!(bid_blind < bid_live, "{bid_blind} !< {bid_live}");
        // Penalty below 1 must not tighten the quote (clamped to neutral).
        let tightening = PricingConfig { fallback_vol_penalty: 0.5, ..pricing_cfg() };
        let ask_clamped = premium_of(&price_rfq(&tightening, &ask_rfq, spot, fallback, 0));
        let ask_neutral = premium_of(&price_rfq(&pricing_cfg(), &ask_rfq, spot, live(0.60), 0));
        assert_eq!(ask_clamped, ask_neutral);
    }

    #[test]
    fn neutral_knobs_reproduce_bps_only_pricing() {
        // Regression guard: with the new knobs at their neutral values the
        // premium must be exactly what the pre-overhaul bps-only model
        // produced (mid ± bps, ask ceiled / bid floored).
        let (ask_rfq, bid_rfq, spot) = weekly_atm();
        let cfg = PricingConfig {
            ask_markup_bps: 100,
            bid_markdown_bps: 200,
            ..pricing_cfg()
        };
        let sigma = 0.60;
        let mid = pricing::call_price_per_unit(CallInputs {
            spot,
            strike: 100.0,
            t_years: time_to_expiry_years(ask_rfq.expiry_ms, 0),
            r: cfg.rate,
            sigma,
        });
        let expected_ask = premium_for_write_ceil(mid * 1.01, 1_000_000);
        let expected_bid = premium_for_write(mid * 0.98, 1_000_000);
        assert_eq!(
            premium_of(&price_rfq(&cfg, &ask_rfq, spot, live(sigma), 0)),
            expected_ask
        );
        assert_eq!(
            premium_of(&price_rfq(&cfg, &bid_rfq, spot, live(sigma), 0)),
            expected_bid
        );
    }

    #[test]
    fn smile_raises_otm_call_ask_and_leaves_atm_alone() {
        let week_ms = 1000 * 86_400 * 7u64;
        let smiled = PricingConfig {
            smile: Smile { skew: 0.1, convexity: 0.0 },
            ..pricing_cfg()
        };
        // Far-OTM call (strike 130 vs spot 100): positive skew richens the wing.
        let otm = rfq_side(Side::Trader, week_ms, 130, 0, 1_000_000);
        let flat_px = premium_of(&price_rfq(&pricing_cfg(), &otm, 100.0, live(0.60), 0));
        let smiled_px = premium_of(&price_rfq(&smiled, &otm, 100.0, live(0.60), 0));
        assert!(smiled_px > flat_px, "{smiled_px} !> {flat_px}");
        // ATM (z = 0): identical premium.
        let atm = rfq_side(Side::Trader, week_ms, 100, 0, 1_000_000);
        assert_eq!(
            premium_of(&price_rfq(&pricing_cfg(), &atm, 100.0, live(0.60), 0)),
            premium_of(&price_rfq(&smiled, &atm, 100.0, live(0.60), 0))
        );
    }

    // -- size cap + size widening ------------------------------------------

    #[test]
    fn oversize_rfq_declines() {
        let (ask_rfq, _, spot) = weekly_atm();
        // Notional = 100 × 1M = 100M; cap it below that.
        let capped = PricingConfig { max_quote_notional: 50_000_000, ..pricing_cfg() };
        match price_rfq(&capped, &ask_rfq, spot, live(0.60), 0) {
            PriceDecision::Decline { reason } => {
                assert_eq!(reason, "size exceeds max quote notional")
            }
            other => panic!("expected Decline, got {other:?}"),
        }
        // At or under the cap quotes fine; 0 disables entirely.
        let roomy = PricingConfig { max_quote_notional: 200_000_000, ..pricing_cfg() };
        assert!(matches!(
            price_rfq(&roomy, &ask_rfq, spot, live(0.60), 0),
            PriceDecision::Quote { .. }
        ));
        assert!(matches!(
            price_rfq(&pricing_cfg(), &ask_rfq, spot, live(0.60), 0),
            PriceDecision::Quote { .. }
        ));
    }

    #[test]
    fn size_widening_makes_big_clips_pay_more_per_unit() {
        let week_ms = 1000 * 86_400 * 7u64;
        let cfg = PricingConfig {
            // 10% extra vol per 100M notional.
            size_widening_vol: 0.10,
            size_ref_notional: 100_000_000,
            ..pricing_cfg()
        };
        let small = rfq_side(Side::Trader, week_ms, 100, 0, 1_000_000); // 100M notional
        let big = rfq_side(Side::Trader, week_ms, 100, 0, 10_000_000); // 1B notional
        let per_unit_of = |d: &PriceDecision| match d {
            PriceDecision::Quote { per_unit, .. } => *per_unit,
            other => panic!("expected Quote, got {other:?}"),
        };
        let small_unit = per_unit_of(&price_rfq(&cfg, &small, 100.0, live(0.60), 0));
        let big_unit = per_unit_of(&price_rfq(&cfg, &big, 100.0, live(0.60), 0));
        assert!(big_unit > small_unit, "{big_unit} !> {small_unit}");
        // Bid side: the big clip is paid less per unit.
        let small_bid = rfq_side(Side::Writer, week_ms, 100, 0, 1_000_000);
        let big_bid = rfq_side(Side::Writer, week_ms, 100, 0, 10_000_000);
        let small_bid_unit = per_unit_of(&price_rfq(&cfg, &small_bid, 100.0, live(0.60), 0));
        let big_bid_unit = per_unit_of(&price_rfq(&cfg, &big_bid, 100.0, live(0.60), 0));
        assert!(big_bid_unit < small_bid_unit, "{big_bid_unit} !< {small_bid_unit}");
        // Neutral knobs: per-unit price is size-independent.
        let flat_small = per_unit_of(&price_rfq(&pricing_cfg(), &small, 100.0, live(0.60), 0));
        let flat_big = per_unit_of(&price_rfq(&pricing_cfg(), &big, 100.0, live(0.60), 0));
        assert!((flat_small - flat_big).abs() < 1e-12);
    }
}
