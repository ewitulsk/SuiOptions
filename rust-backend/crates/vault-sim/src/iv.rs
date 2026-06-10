//! IV proxies (doc 06 §4). The product has no implied-vol history, so IV
//! is always a *model*: each provider derives an annualized vol for the
//! (tenor, delta) being priced from data that does exist. Providers carry
//! their own aligned series; `MarketCtx` only exposes the path.

use crate::paths::Bar;

/// What a provider may look at when quoting IV at bar `idx`: the path up
/// to and including `idx` (no lookahead) and, for synthetic paths that
/// know it, the generator's own per-bar vol.
pub struct MarketCtx<'a> {
    pub bars: &'a [Bar],
    pub idx: usize,
    /// `Path::sigma_ann[idx]`, when the generator supplies it.
    pub path_sigma: Option<f64>,
}

pub trait IvProvider {
    /// Annualized IV for an option of `tenor_years` at `delta`, quoted at
    /// `ctx.idx` along the path.
    fn iv(&self, ctx: &MarketCtx, tenor_years: f64, delta: f64) -> f64;
}

/// Annualized close-to-close realized vol over the trailing `window` bars
/// ending at `idx`. Sample stddev (N−1) of log returns × √bars_per_year —
/// the same convention as `pyth-client::vol::realized_vol`, reimplemented
/// here so the sim crate stays free of service dependencies.
pub fn realized_vol(bars: &[Bar], idx: usize, window: usize, bars_per_year: f64) -> f64 {
    let start = idx.saturating_sub(window);
    let closes: Vec<f64> = bars[start..=idx].iter().map(|b| b.close).collect();
    if closes.len() < 3 {
        return 0.0;
    }
    let rets: Vec<f64> = closes.windows(2).map(|w| (w[1] / w[0]).ln()).collect();
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
    (var * bars_per_year).sqrt()
}

/// Floor scenario: IV = realized vol, no premium for uncertainty. Uses the
/// generator's path-consistent sigma when available.
pub struct RealizedOnly {
    pub window: usize,
    pub bars_per_year: f64,
}

impl IvProvider for RealizedOnly {
    fn iv(&self, ctx: &MarketCtx, _tenor_years: f64, _delta: f64) -> f64 {
        if let Some(s) = ctx.path_sigma {
            return s;
        }
        realized_vol(ctx.bars, ctx.idx, self.window, self.bars_per_year)
    }
}

/// VRP transfer (headline for SUI): own realized vol × a configured
/// IV/RV ratio borrowed from a reference market (median DVOL/RV₃₀ for BTC,
/// calibrated by the backtester's data runs — doc 06 §5).
pub struct VrpTransfer {
    pub window: usize,
    pub bars_per_year: f64,
    /// Reference market's median IV/RV (e.g. 1.15).
    pub vrp_ratio: f64,
}

impl IvProvider for VrpTransfer {
    fn iv(&self, ctx: &MarketCtx, _tenor_years: f64, _delta: f64) -> f64 {
        let rv = ctx
            .path_sigma
            .unwrap_or_else(|| realized_vol(ctx.bars, ctx.idx, self.window, self.bars_per_year));
        rv * self.vrp_ratio
    }
}

/// Beta-scaled: the reference market's *live* IV series scaled by the
/// relative realized vol over `window`: (RV_own / RV_ref) × IV_ref. Needs
/// bar-aligned reference series, so it is replay-only.
pub struct BetaScaled {
    pub window: usize,
    pub bars_per_year: f64,
    /// Reference IV (e.g. BTC DVOL, as a fraction) aligned to the path.
    pub ref_iv: Vec<f64>,
    /// Reference market closes aligned to the path.
    pub ref_closes: Vec<f64>,
}

impl IvProvider for BetaScaled {
    fn iv(&self, ctx: &MarketCtx, _tenor_years: f64, _delta: f64) -> f64 {
        let own_rv = realized_vol(ctx.bars, ctx.idx, self.window, self.bars_per_year);
        let ref_bars: Vec<Bar> = self
            .ref_closes
            .iter()
            .enumerate()
            .map(|(i, c)| Bar::flat(i as u64, *c))
            .collect();
        let ref_rv = realized_vol(&ref_bars, ctx.idx, self.window, self.bars_per_year);
        if ref_rv <= 0.0 {
            return own_rv;
        }
        (own_rv / ref_rv) * self.ref_iv[ctx.idx]
    }
}

/// Direct index IV (BTC/ETH replay): the asset's own DVOL series with a
/// skew adjustment mapping the ATM index to the priced wing (10Δ trades
/// above ATM IV in upside-skewed crypto regimes; `skew_bps` is swept).
pub struct DeribitDvol {
    /// DVOL as a fraction (e.g. 0.55), aligned to the path bars.
    pub dvol: Vec<f64>,
    /// Adjustment in bps of the index: iv = dvol × (1 + skew_bps/10⁴).
    pub skew_bps: i64,
}

impl IvProvider for DeribitDvol {
    fn iv(&self, ctx: &MarketCtx, _tenor_years: f64, _delta: f64) -> f64 {
        self.dvol[ctx.idx] * (1.0 + self.skew_bps as f64 / 10_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Bar;

    fn flat_bars(n: usize, p: f64) -> Vec<Bar> {
        (0..n).map(|i| Bar::flat(i as u64, p)).collect()
    }

    #[test]
    fn realized_vol_zero_for_constant_prices() {
        let bars = flat_bars(40, 100.0);
        assert_eq!(realized_vol(&bars, 39, 30, 365.0), 0.0);
    }

    #[test]
    fn realized_vol_matches_hand_computation() {
        // Alternating ±1% closes: log returns ±ln(1.01…)-ish, exactly
        // computable.
        let mut bars = Vec::new();
        let mut p = 100.0;
        for i in 0..31 {
            bars.push(Bar::flat(i as u64, p));
            p *= if i % 2 == 0 { 1.01 } else { 1.0 / 1.01 };
        }
        let rets: Vec<f64> =
            bars.windows(2).map(|w| (w[1].close / w[0].close).ln()).collect();
        let mean = rets.iter().sum::<f64>() / rets.len() as f64;
        let var =
            rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
        let expected = (var * 365.0).sqrt();
        assert!((realized_vol(&bars, 30, 30, 365.0) - expected).abs() < 1e-12);
    }

    #[test]
    fn providers_compose_rv_as_documented() {
        let mut bars = Vec::new();
        let mut p = 100.0;
        for i in 0..40 {
            bars.push(Bar::flat(i as u64, p));
            p *= 1.0 + 0.02 * ((i as f64).sin());
        }
        let ctx = MarketCtx { bars: &bars, idx: 39, path_sigma: None };
        let rv = realized_vol(&bars, 39, 30, 365.0);

        let vrp = VrpTransfer { window: 30, bars_per_year: 365.0, vrp_ratio: 1.15 };
        assert!((vrp.iv(&ctx, 7.0 / 365.0, 0.10) - rv * 1.15).abs() < 1e-12);

        let floor = RealizedOnly { window: 30, bars_per_year: 365.0 };
        assert!((floor.iv(&ctx, 7.0 / 365.0, 0.10) - rv).abs() < 1e-12);

        // Beta-scaled with the asset itself as reference and a flat 0.5
        // index: (rv/rv) × 0.5 = 0.5.
        let beta = BetaScaled {
            window: 30,
            bars_per_year: 365.0,
            ref_iv: vec![0.5; 40],
            ref_closes: bars.iter().map(|b| b.close).collect(),
        };
        assert!((beta.iv(&ctx, 7.0 / 365.0, 0.10) - 0.5).abs() < 1e-12);

        let dvol = DeribitDvol { dvol: vec![0.5; 40], skew_bps: 1000 };
        assert!((dvol.iv(&ctx, 7.0 / 365.0, 0.10) - 0.55).abs() < 1e-12);
    }

    #[test]
    fn path_sigma_takes_precedence() {
        let bars = flat_bars(40, 100.0);
        let ctx = MarketCtx { bars: &bars, idx: 39, path_sigma: Some(0.8) };
        let floor = RealizedOnly { window: 30, bars_per_year: 365.0 };
        assert_eq!(floor.iv(&ctx, 7.0 / 365.0, 0.10), 0.8);
    }
}
