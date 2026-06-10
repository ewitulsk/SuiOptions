//! Strike-grid computation.
//!
//! Translates `(spot_usd_cross, under_dec, settle_dec, strikes_below,
//! strikes_above, interval_pct)` into `(start_strike, strike_interval,
//! count, strike_scale)` in **scaled chain strike units** — i.e. the same
//! shape `bucket::new_call_option` expects after SO-55.
//!
//! ## Chain units (post-SO-55)
//!
//! On-chain `strike: u128` is the *scaled* ratio. The real settlement
//! smallest-units owed per underlying smallest-unit is `strike /
//! 10^strike_scale`. Putting the divisor in a per-bucket field lets a
//! sub-cent asset paired against a same-decimal stablecoin (e.g.
//! TDEEP/TUSDC at $0.15 with both 6-dec) carry meaningful resolution that
//! a plain integer ratio cannot.
//!
//! ## Auto-derived scale
//!
//! [`build_strike_grid_for_pair`] picks the smallest `strike_scale` in
//! `[0, MAX_STRIKE_SCALE]` such that the resulting `strike_interval` is at
//! least `TARGET_INTERVAL_CHAIN_UNITS` (= 1000). That gives three digits
//! of sub-interval resolution, so the round-half-up math in
//! `bucket::apply_strike` doesn't degenerate even for tiny exercises. If
//! no scale in range hits the target, we accept the largest scale that
//! still produces a non-zero interval; if even that's zero, we error.
//!
//! Worked examples:
//! - TBTC(8)/TUSDC(6) at $77 000, 5% interval → scale=2, spot_chain=77 000,
//!   interval=3 850.
//! - TDEEP(6)/TUSDC(6) at $0.15, 10% interval → scale=5, spot_chain=15 000,
//!   interval=1 500.

use anyhow::{anyhow, bail, Result};
use tracing::debug;

/// Caps `strike_scale` at the same upper bound the on-chain `pow10` does
/// (`bucket.move::MAX_STRIKE_SCALE`). 38 is the largest exponent for which
/// `10^scale` still fits in u128; the picker also guards against the
/// `spot × 10^(scale + dec_diff)` product overflowing u128, so we'll
/// naturally fall back to a smaller scale for high-value pairs.
pub const MAX_STRIKE_SCALE: u8 = 38;

/// Minimum chain-units we want each strike interval to span. Three digits
/// gives `round_half_up` enough room that an exercise of one underlying
/// smallest-unit settles to a meaningful integer instead of 0.
const TARGET_INTERVAL_CHAIN_UNITS: u128 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrikeGrid {
    pub start_strike: u128,
    pub strike_interval: u128,
    pub count: u64,
    pub strike_scale: u8,
}

impl StrikeGrid {
    /// Expand the uniform grid into explicit per-bucket strikes — the
    /// shape `RollPlan` carries now that grids may be non-uniform.
    pub fn strikes(&self) -> Vec<u128> {
        (0..self.count)
            .map(|i| self.start_strike + (i as u128) * self.strike_interval)
            .collect()
    }
}

/// Legacy helper kept for tests / dev convenience: convert a USD spot
/// into the scale=0 chain-unit ratio.
pub fn spot_usd_to_chain_units(
    spot_usd_cross: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<u128> {
    if !spot_usd_cross.is_finite() || spot_usd_cross <= 0.0 {
        return Err(anyhow!(
            "spot price must be positive and finite: {spot_usd_cross}"
        ));
    }
    let exp = settlement_decimals as i32 - underlying_decimals as i32;
    let raw = spot_usd_cross * 10f64.powi(exp);
    if !raw.is_finite() || raw < 0.0 {
        return Err(anyhow!("spot conversion overflowed: {raw}"));
    }
    if raw >= u128::MAX as f64 {
        return Err(anyhow!("spot conversion exceeds u128::MAX: {raw}"));
    }
    Ok(raw.floor() as u128)
}

/// Build the strike grid for a pair, auto-deriving `strike_scale` so the
/// interval lands in a comfortable resolution band. See the module-level
/// doc for the algorithm.
///
/// `spot_usd_cross` is the *cross* settlement-per-underlying in USD
/// terms — for a stablecoin settlement it collapses to the underlying's
/// USD price.
pub fn build_strike_grid_for_pair(
    spot_usd_cross: f64,
    underlying_decimals: u8,
    settlement_decimals: u8,
    strikes_below: u32,
    strikes_above: u32,
    interval_pct: f64,
) -> Result<StrikeGrid> {
    if !spot_usd_cross.is_finite() || spot_usd_cross <= 0.0 {
        bail!("spot must be positive and finite: {spot_usd_cross}");
    }
    if !interval_pct.is_finite() || interval_pct <= 0.0 {
        bail!("interval_pct must be positive: {interval_pct}");
    }

    let dec_diff = settlement_decimals as i32 - underlying_decimals as i32;

    // Walk scales 0..=MAX. The first one that meets the resolution target
    // wins; otherwise we remember the last *viable* (interval ≥ 1) scale
    // so degenerate but workable pairs still get a grid.
    let mut chosen: Option<(u8, u128, u128)> = None;
    for scale in 0u8..=MAX_STRIKE_SCALE {
        let exp = scale as i32 + dec_diff;
        let spot_chain_f = spot_usd_cross * 10f64.powi(exp);
        if !spot_chain_f.is_finite() || spot_chain_f < 0.0 || spot_chain_f >= u128::MAX as f64 {
            continue;
        }
        let spot_chain = spot_chain_f.floor() as u128;
        let interval = (spot_chain_f * interval_pct / 100.0).floor() as u128;
        if spot_chain < 1 || interval < 1 {
            continue;
        }
        chosen = Some((scale, spot_chain, interval));
        if interval >= TARGET_INTERVAL_CHAIN_UNITS {
            break;
        }
    }
    let (strike_scale, spot_chain, strike_interval) = chosen.ok_or_else(|| {
        anyhow!(
            "no viable strike_scale in [0, {MAX_STRIKE_SCALE}]: \
             spot_usd_cross={spot_usd_cross} interval_pct={interval_pct}% \
             under_dec={underlying_decimals} settle_dec={settlement_decimals}"
        )
    })?;

    let start_strike_i =
        (spot_chain as i128) - (strikes_below as i128) * (strike_interval as i128);
    if start_strike_i <= 0 {
        bail!(
            "start_strike non-positive ({start_strike_i}) — strikes_below too large \
             for spot_chain={spot_chain}, interval={strike_interval}"
        );
    }
    let count = (strikes_below as u64) + (strikes_above as u64) + 1;
    debug!(
        spot_usd_cross,
        strike_scale,
        spot_chain,
        start_strike = start_strike_i,
        strike_interval,
        count,
        interval_pct,
        "computed strike grid"
    );
    Ok(StrikeGrid {
        start_strike: start_strike_i as u128,
        strike_interval,
        count,
        strike_scale,
    })
}

/// Grid v2 (doc 05 §4.2): strikes at fixed standard-deviation multiples
/// `K_i = round_nice(S · exp(z_i · σ · √τ))` instead of fixed percentages,
/// so the vault's 0.1-delta strike (z = 1.30) is on-grid by construction
/// and spacing adapts to the vol regime. The f64 ladder math is shared
/// with the keeper and the backtester via `pricing::grid`.
///
/// Returns explicit per-bucket strikes plus the auto-derived scale. The
/// scale-picker mirrors [`build_strike_grid_for_pair`]'s, applied to the
/// *smallest interval between adjacent strikes* (the grid is no longer
/// uniform): the first scale where that gap reaches
/// `TARGET_INTERVAL_CHAIN_UNITS` wins, with the largest workable scale as
/// fallback.
pub fn build_z_ladder_for_pair(
    spot_usd_cross: f64,
    sigma: f64,
    tau_years: f64,
    ladder: &[f64],
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Result<(Vec<u128>, u8)> {
    if !spot_usd_cross.is_finite() || spot_usd_cross <= 0.0 {
        bail!("spot must be positive and finite: {spot_usd_cross}");
    }
    if !sigma.is_finite() || sigma <= 0.0 {
        bail!("sigma must be positive and finite: {sigma}");
    }
    if !tau_years.is_finite() || tau_years <= 0.0 {
        bail!("tau_years must be positive and finite: {tau_years}");
    }
    if ladder.len() < 2 {
        bail!("ladder needs at least 2 strikes, got {}", ladder.len());
    }

    let usd_strikes = pricing::grid::build_z_ladder(spot_usd_cross, sigma, tau_years, ladder);
    let dec_diff = settlement_decimals as i32 - underlying_decimals as i32;

    let mut chosen: Option<(u8, Vec<u128>, u128)> = None;
    for scale in 0u8..=MAX_STRIKE_SCALE {
        let exp = scale as i32 + dec_diff;
        let factor = 10f64.powi(exp);
        let mut chain: Vec<u128> = Vec::with_capacity(usd_strikes.len());
        let mut viable = true;
        for k in &usd_strikes {
            let v = k * factor;
            if !v.is_finite() || v < 1.0 || v >= u128::MAX as f64 {
                viable = false;
                break;
            }
            let v = v.round() as u128;
            // Rounding must not collapse adjacent strikes.
            if chain.last().is_some_and(|prev| *prev >= v) {
                viable = false;
                break;
            }
            chain.push(v);
        }
        if !viable {
            continue;
        }
        let min_gap = chain
            .windows(2)
            .map(|w| w[1] - w[0])
            .min()
            .expect("ladder has ≥ 2 strikes");
        chosen = Some((scale, chain, min_gap));
        if min_gap >= TARGET_INTERVAL_CHAIN_UNITS {
            break;
        }
    }
    let (strike_scale, strikes, min_gap) = chosen.ok_or_else(|| {
        anyhow!(
            "no viable strike_scale in [0, {MAX_STRIKE_SCALE}] for z-ladder: \
             spot_usd_cross={spot_usd_cross} sigma={sigma} tau_years={tau_years} \
             under_dec={underlying_decimals} settle_dec={settlement_decimals}"
        )
    })?;
    debug!(
        spot_usd_cross,
        sigma,
        tau_years,
        strike_scale,
        min_gap,
        strikes = ?strikes,
        "computed z-ladder strike grid"
    );
    Ok((strikes, strike_scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btc_usdc_77k_picks_scale_2() {
        // TBTC(8) / TUSDC(6) at ~$77k, 5% interval. At scale=0 the
        // interval would only be 38 chain units — well under the 1000
        // resolution target. scale=2 gives us 3850.
        let g = build_strike_grid_for_pair(77_000.0, 8, 6, 4, 4, 5.0).unwrap();
        assert_eq!(g.strike_scale, 2);
        assert_eq!(g.strike_interval, 3850);
        // spot_chain at scale=2 is 77000 → start = 77000 - 4×3850 = 61600.
        assert_eq!(g.start_strike, 61_600);
        assert_eq!(g.count, 9);
    }

    #[test]
    fn deep_usdc_15_cents_now_works() {
        // TDEEP(6) / TUSDC(6) at $0.15, 10% interval. Pre-SO-55 this
        // bailed with "spot is 0 in chain units". Post: auto-derive picks
        // scale=5 (spot_chain=15_000, interval=1_500).
        let g = build_strike_grid_for_pair(0.15, 6, 6, 2, 2, 10.0).unwrap();
        assert_eq!(g.strike_scale, 5);
        assert_eq!(g.strike_interval, 1_500);
        assert_eq!(g.start_strike, 15_000 - 2 * 1_500);
        assert_eq!(g.count, 5);
    }

    #[test]
    fn fallback_to_largest_viable_scale_when_target_unreachable() {
        // Tuned so even at scale=MAX (38) the interval is < 1000 but
        // still ≥ 1 — picker exhausts the loop and returns scale=MAX
        // with that workable-but-coarse interval rather than erroring.
        //
        // Math (spot=$1, dec_diff=-2 → spot_chain at scale=38 = 1e36;
        // interval = 1e36 × 1e-33 / 100 = 10).
        let g = build_strike_grid_for_pair(1.0, 8, 6, 2, 2, 1e-33).unwrap();
        assert_eq!(g.strike_scale, MAX_STRIKE_SCALE);
        assert!(g.strike_interval >= 1);
        assert!(g.strike_interval < TARGET_INTERVAL_CHAIN_UNITS);
    }

    #[test]
    fn picker_exits_at_first_scale_meeting_target() {
        // BTC at $77k, 1% interval. scale=2 gives interval=770 (<1000),
        // scale=3 gives interval=7700 (≥1000). Picker should stop at 3,
        // not greedily pick a higher scale.
        let g = build_strike_grid_for_pair(77_000.0, 8, 6, 2, 2, 1.0).unwrap();
        assert_eq!(g.strike_scale, 3);
        assert_eq!(g.strike_interval, 7_700);
    }

    #[test]
    fn rejects_when_no_scale_yields_nonzero_interval() {
        // Implausibly small spot — even at scale=MAX (38), spot_chain
        // (1e-45 × 1e38 = 1e-7) underflows to 0 so no scale produces a
        // viable grid. Picker errors rather than submitting a no-op
        // family.
        let err = build_strike_grid_for_pair(1e-45, 6, 6, 2, 2, 1.0).unwrap_err();
        assert!(err.to_string().contains("no viable strike_scale"), "{err}");
    }

    #[test]
    fn rejects_zero_or_negative_spot() {
        assert!(build_strike_grid_for_pair(0.0, 8, 6, 2, 2, 5.0).is_err());
        assert!(build_strike_grid_for_pair(-1.0, 8, 6, 2, 2, 5.0).is_err());
        assert!(build_strike_grid_for_pair(f64::NAN, 8, 6, 2, 2, 5.0).is_err());
    }

    #[test]
    fn rejects_zero_or_negative_interval_pct() {
        assert!(build_strike_grid_for_pair(50_000.0, 8, 6, 2, 2, 0.0).is_err());
        assert!(build_strike_grid_for_pair(50_000.0, 8, 6, 2, 2, -1.0).is_err());
        assert!(build_strike_grid_for_pair(50_000.0, 8, 6, 2, 2, f64::INFINITY).is_err());
    }

    #[test]
    fn rejects_start_strike_below_zero() {
        // 1000 strikes below at $77k → walks past zero, regardless of scale.
        let err = build_strike_grid_for_pair(77_000.0, 8, 6, 1000, 0, 5.0).unwrap_err();
        assert!(err.to_string().contains("non-positive"), "{err}");
    }

    #[test]
    fn grid_strikes_expand_uniformly() {
        let g = StrikeGrid {
            start_strike: 100,
            strike_interval: 25,
            count: 4,
            strike_scale: 2,
        };
        assert_eq!(g.strikes(), vec![100, 125, 150, 175]);
    }

    #[test]
    fn z_ladder_sui_usdc_hits_resolution_target() {
        // SUI(9)/USDC(6) at $3.47, 85% vol, weekly tenor. dec_diff = −3,
        // so chain values start around 3.47e-3 — the picker must walk the
        // scale up until adjacent gaps clear 1000 chain units.
        let (strikes, scale) =
            build_z_ladder_for_pair(3.47, 0.85, 7.0 / 365.0, &pricing::grid::SUI_LADDER, 9, 6)
                .unwrap();
        assert_eq!(strikes.len(), 5);
        for w in strikes.windows(2) {
            assert!(w[1] > w[0], "not increasing: {strikes:?}");
            assert!(w[1] - w[0] >= TARGET_INTERVAL_CHAIN_UNITS, "gap too small: {strikes:?}");
        }
        // The z = 1.30 vault-target strike sits above spot.
        let spot_chain = 3.47 * 10f64.powi(scale as i32 - 3);
        assert!((strikes[2] as f64) > spot_chain);
        // ATM (z = 0) strike is within a nice-rounding step of spot.
        assert!(((strikes[0] as f64) - spot_chain).abs() / spot_chain < 0.01);
    }

    #[test]
    fn z_ladder_btc_seven_strikes_with_itm_wing() {
        let (strikes, scale) = build_z_ladder_for_pair(
            117_483.91,
            0.55,
            7.0 / 365.0,
            &pricing::grid::BTC_LADDER,
            8,
            6,
        )
        .unwrap();
        assert_eq!(strikes.len(), 7);
        let spot_chain = 117_483.91 * 10f64.powi(scale as i32 - 2);
        assert!((strikes[0] as f64) < spot_chain, "z=-0.65 wing is ITM");
        assert!((strikes[6] as f64) > spot_chain * 1.1, "far OTM wing");
        for w in strikes.windows(2) {
            assert!(w[1] - w[0] >= TARGET_INTERVAL_CHAIN_UNITS);
        }
    }

    #[test]
    fn z_ladder_spacing_tracks_vol() {
        let tau = 7.0 / 365.0;
        let ladder = pricing::grid::SUI_LADDER;
        let (calm, s1) = build_z_ladder_for_pair(3.47, 0.30, tau, &ladder, 9, 6).unwrap();
        let (wild, s2) = build_z_ladder_for_pair(3.47, 1.20, tau, &ladder, 9, 6).unwrap();
        assert_eq!(s1, s2, "same magnitude ⇒ same scale");
        let width = |v: &[u128]| v[4] as f64 / v[0] as f64 - 1.0;
        assert!(width(&wild) > width(&calm) * 3.0);
    }

    #[test]
    fn z_ladder_rejects_bad_inputs() {
        let ladder = pricing::grid::SUI_LADDER;
        assert!(build_z_ladder_for_pair(0.0, 0.8, 0.02, &ladder, 9, 6).is_err());
        assert!(build_z_ladder_for_pair(3.5, 0.0, 0.02, &ladder, 9, 6).is_err());
        assert!(build_z_ladder_for_pair(3.5, 0.8, 0.0, &ladder, 9, 6).is_err());
        assert!(build_z_ladder_for_pair(3.5, 0.8, 0.02, &[1.3], 9, 6).is_err());
    }

    #[test]
    fn spot_usd_to_chain_units_round_trip() {
        assert_eq!(spot_usd_to_chain_units(50_000.0, 8, 6).unwrap(), 500);
        assert_eq!(spot_usd_to_chain_units(0.15, 6, 9).unwrap(), 150);
        // Sub-1 ratio floors to 0 at scale=0; build_strike_grid_for_pair
        // is the path callers want for that case.
        assert_eq!(spot_usd_to_chain_units(0.15, 6, 6).unwrap(), 0);
    }
}
