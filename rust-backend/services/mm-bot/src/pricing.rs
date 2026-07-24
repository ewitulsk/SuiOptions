//! Pure market-data chassis helpers shared by the desk's flows: strike
//! rescaling, time-to-expiry, realized-vol selection with a fallback, and
//! the pair-match gate.
//!
//! Spot construction from Pyth prices (with staleness/confidence checks)
//! moved to `pyth_client::spot` (SO-302) so the market-sim service shares
//! it; the names below re-export from there so desk call sites are
//! unchanged.
//!
//! The old vol-markup/smile quote model (`price_rfq` and friends) died in
//! the SO-299 strategy reset — quote pricing now lives in `desk::` on top
//! of the `crates/pricing` surface/american/desk modules.

use protocol_types::asset::canonicalize_move_type;

pub use pyth_client::{compute_spot_from_cache, compute_spot_from_prices, SpotError, Staleness};

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

    // A representative configured pair for the `serves_pair` tests below.
    const UNDERLYING: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tbtc::TBTC";
    const SETTLEMENT: &str = "0x9b72409a9f38a8784420d17577aa6dbe5aa2ab4224cd04c44d8b515f6c97ba86::tusdc::TUSDC";

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    // The compute_spot_* / Staleness / SpotError tests moved to
    // `pyth_client::spot` with the code (SO-302).

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
