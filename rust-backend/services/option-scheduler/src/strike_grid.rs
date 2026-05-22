//! Strike-grid computation.
//!
//! Translates `(spot_usd, strikes_below, strikes_above, interval_pct)` into
//! `(start_strike, strike_interval, count)` in **chain strike units**.
//!
//! ## Chain units
//!
//! The Move contract's `strike: u64` is in settlement smallest-units per
//! underlying smallest-unit. The conversion is the same one `mm-bot` uses
//! and is documented at `rust-backend/README.md` (and on the on-chain side
//! at `contracts/sources/bucket.move`):
//!
//! ```text
//!   strike_chain = spot_usd × 10^settlement_decimals / 10^underlying_decimals
//! ```
//!
//! For TBTC (8 dec) / TUSDC (6 dec) at $50k:
//!   `50_000 × 10^6 / 10^8 = 500`.
//!
//! For TDEEP (6 dec) / TUSDC (6 dec) at $0.15:
//!   `0.15 × 10^6 / 10^6 = 0.15` — rounds to 0, which is why an asset whose
//!   chain price would round to <1 needs `interval_pct` to produce
//!   ≥1-unit steps (or the bucket grid degenerates). The function returns
//!   an error in that case so the bot doesn't silently submit a no-op.

use anyhow::{anyhow, Result};
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrikeGrid {
    pub start_strike: u64,
    pub strike_interval: u64,
    pub count: u64,
}

/// Convert a USD spot price into the chain's u64 strike unit. Floors the
/// fractional remainder, matching the bucket's u64 type.
pub fn spot_usd_to_chain_units(
    spot_usd: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<u64> {
    if !spot_usd.is_finite() || spot_usd <= 0.0 {
        return Err(anyhow!("spot price must be positive and finite: {spot_usd}"));
    }
    let pow_settle = 10f64.powi(settlement_decimals as i32);
    let pow_under = 10f64.powi(underlying_decimals as i32);
    let raw = spot_usd * pow_settle / pow_under;
    if !raw.is_finite() || raw < 0.0 {
        return Err(anyhow!("spot conversion overflowed: {raw}"));
    }
    if raw >= u64::MAX as f64 {
        return Err(anyhow!("spot conversion exceeds u64::MAX: {raw}"));
    }
    let chain = raw.floor() as u64;
    debug!(spot_usd, underlying_decimals, settlement_decimals, chain, "spot usd to chain units");
    Ok(chain)
}

/// Build a `(start, interval, count)` strike grid from a pre-scaled
/// chain-unit spot.
///
/// This is the kernel both the static and Pyth spot sources call into.
/// Static computes `spot_chain` from `usd × 10^(settle - under)`; Pyth
/// computes it from the cross of two live USD prices. Either way, this
/// function only sees a u64.
///
/// The interval is `spot_chain * interval_pct / 100`, floored. The start
/// strike sits `strikes_below` steps below spot; total `count =
/// strikes_below + strikes_above + 1` so spot lands on the middle strike
/// (subject to flooring).
pub fn build_strike_grid_from_chain(
    spot_chain: u64,
    strikes_below: u32,
    strikes_above: u32,
    interval_pct: f64,
) -> Result<StrikeGrid> {
    if spot_chain == 0 {
        return Err(anyhow!(
            "spot is 0 in chain units — pick a settlement with more decimals or \
             a non-test underlying"
        ));
    }
    if !(interval_pct.is_finite() && interval_pct > 0.0) {
        return Err(anyhow!("interval_pct must be positive: {interval_pct}"));
    }
    let interval_f = spot_chain as f64 * interval_pct / 100.0;
    let strike_interval = interval_f.floor() as u64;
    if strike_interval == 0 {
        return Err(anyhow!(
            "interval_pct={interval_pct}% at spot_chain={spot_chain} rounds to a \
             zero strike interval — widen the spacing or pick a more granular \
             settlement"
        ));
    }
    let start_strike = (spot_chain as i128) - (strikes_below as i128) * (strike_interval as i128);
    if start_strike <= 0 {
        return Err(anyhow!(
            "start_strike non-positive ({start_strike}) — strikes_below too large \
             for spot_chain={spot_chain}, interval={strike_interval}"
        ));
    }
    let count = (strikes_below as u64) + (strikes_above as u64) + 1;
    debug!(
        spot_chain,
        start_strike,
        strike_interval,
        count,
        interval_pct,
        "computed strike grid"
    );
    Ok(StrikeGrid {
        start_strike: start_strike as u64,
        strike_interval,
        count,
    })
}

/// Convenience wrapper for the static spot path: USD → chain units →
/// grid. Pyth callers go through [`build_strike_grid_from_chain`]
/// directly since they already have the chain-unit spot.
pub fn build_strike_grid(
    spot_usd: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
    strikes_below: u32,
    strikes_above: u32,
    interval_pct: f64,
) -> Result<StrikeGrid> {
    let spot_chain = spot_usd_to_chain_units(spot_usd, underlying_decimals, settlement_decimals)?;
    build_strike_grid_from_chain(spot_chain, strikes_below, strikes_above, interval_pct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btc_usdc_50k_round_trip() {
        // 8-dec underlying, 6-dec settlement, $50k spot → 500 chain units.
        let s = spot_usd_to_chain_units(50_000.0, 8, 6).unwrap();
        assert_eq!(s, 500);

        // ±4 strikes at 5% spacing → step = 25, start = 400, count = 9.
        let g = build_strike_grid(50_000.0, 8, 6, 4, 4, 5.0).unwrap();
        assert_eq!(g.strike_interval, 25);
        assert_eq!(g.start_strike, 400);
        assert_eq!(g.count, 9);
        // Highest strike on the grid: start + (count-1)*interval = 400 + 8*25 = 600.
        let highest = g.start_strike + (g.count - 1) * g.strike_interval;
        assert_eq!(highest, 600);
    }

    #[test]
    fn deep_usdc_15_cents_rounds_to_zero_chain() {
        // 6-dec underlying, 6-dec settlement, $0.15 → 0.15 in chain units → 0 after floor.
        let s = spot_usd_to_chain_units(0.15, 6, 6).unwrap();
        assert_eq!(s, 0);

        // build_strike_grid should error rather than silently submit a no-op.
        let err = build_strike_grid(0.15, 6, 6, 2, 2, 10.0).unwrap_err();
        assert!(err.to_string().contains("spot is 0 in chain units"), "{err}");
    }

    #[test]
    fn symmetric_grid_count() {
        let g = build_strike_grid(50_000.0, 8, 6, 4, 4, 5.0).unwrap();
        assert_eq!(g.count, 9);

        let g = build_strike_grid(50_000.0, 8, 6, 2, 6, 5.0).unwrap();
        // 2 below + 6 above + 1 spot = 9
        assert_eq!(g.count, 9);
        // start = spot - 2*step
        assert_eq!(g.start_strike, 500 - 2 * 25);
    }

    #[test]
    fn rejects_zero_step() {
        // 0.01% at spot=500 chain units → 0.05 → floor 0
        let err = build_strike_grid(50_000.0, 8, 6, 4, 4, 0.01).unwrap_err();
        assert!(err.to_string().contains("zero strike interval"), "{err}");
    }

    #[test]
    fn rejects_negative_start() {
        // 1000 strikes below at 5% would walk start past zero.
        let err = build_strike_grid(50_000.0, 8, 6, 1000, 0, 5.0).unwrap_err();
        assert!(err.to_string().contains("non-positive"), "{err}");
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(spot_usd_to_chain_units(0.0, 8, 6).is_err());
        assert!(spot_usd_to_chain_units(-1.0, 8, 6).is_err());
        assert!(spot_usd_to_chain_units(f64::NAN, 8, 6).is_err());
        assert!(build_strike_grid(50_000.0, 8, 6, 4, 4, 0.0).is_err());
        assert!(build_strike_grid(50_000.0, 8, 6, 4, 4, f64::INFINITY).is_err());
    }

    #[test]
    fn higher_settlement_precision_unlocks_low_priced_assets() {
        // If settlement has 9 dec and underlying has 6 dec, $0.15 → 150 chain units.
        let s = spot_usd_to_chain_units(0.15, 6, 9).unwrap();
        assert_eq!(s, 150);
        let g = build_strike_grid(0.15, 6, 9, 2, 2, 10.0).unwrap();
        // step = floor(150 * 10 / 100) = 15
        assert_eq!(g.strike_interval, 15);
        assert_eq!(g.start_strike, 150 - 2 * 15);
        assert_eq!(g.count, 5);
    }
}
