//! Realized-vol-anchored volatility surface — the mm-bot vol desk's model
//! vol (docs/mm-bot-v2/00-plan.md, Phase 1).
//!
//! The base ATM sigma comes from short-horizon realized-vol windows (EWMA
//! over e.g. 1d/7d/30d), blended **MAX-leaning**: take the weighted mean of
//! the live windows, then lift it to `max(weighted_mean, 0.8·max_window)` so
//! a vol spike in ANY single window raises the whole surface even while the
//! longer windows still average it away. The asymmetry is deliberate: buying
//! vol too cheap right after a spike is the expensive mistake; quoting
//! slightly rich for a while after one is not. On top of that base:
//!
//! - `risk_premium` — additive vol points (the desk is a mildly
//!   adversely-selected buyer of retail flow, plan §V1 item 1);
//! - `anchor_ratio` — optional multiplicative scale from an external surface
//!   anchor (BTC/ETH listed surfaces scaled by vol ratio, fast-follow behind
//!   a trait); `None` = off;
//! - `[floor_vol, cap_vol]` clamp so a bad feed can't price vol to zero or
//!   to absurdity.
//!
//! Strike/tenor shape reuses the `smile.rs` prior semantics:
//!
//! ```text
//! σ(K, τ) = σ_atm · term(τ) · clamp(1 + skew·z + convexity·z², 0.25, 4.0)
//! z       = ln(K/S) / (σ_atm·√τ)
//! term(τ) = 1 + term_short_boost·exp(−τ/term_decay_years)
//! ```
//!
//! `term` captures short-dated vol trading over realized; the boost e-folds
//! away over `term_decay_years`. The multiplier clamp is the same [0.25, 4.0]
//! as `smile.rs`, for the same reason: a mis-calibrated wing must never price
//! vol to zero.

/// One realized-vol window observation (e.g. the 1d EWMA). `None` = the
/// window is still cold (not enough samples to trust). `weight` is the
/// window's share of the weighted mean; windows with `weight <= 0` are
/// ignored entirely.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowSample {
    /// Annualized realized vol (e.g. 0.6 for 60%), or `None` while cold.
    pub annualized_vol: Option<f64>,
    /// Blend weight (relative; the mean normalizes by the live-window sum).
    pub weight: f64,
}

/// Surface shape parameters. All vols are annualized decimals (0.05 = 5 vol
/// points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceParams {
    /// Additive risk premium over realized, in vol points (e.g. 0.05).
    pub risk_premium: f64,
    /// Linear smile coefficient per unit z (same semantics as `smile.rs`).
    pub skew: f64,
    /// Quadratic smile coefficient per z² (same semantics as `smile.rs`).
    pub convexity: f64,
    /// Extra vol multiplier at τ → 0: term(0) = 1 + term_short_boost.
    pub term_short_boost: f64,
    /// E-folding time (years) of the short-tenor boost. `<= 0` disables it.
    pub term_decay_years: f64,
    /// External surface anchor scale (BTC/ETH surface × vol ratio); the base
    /// sigma is multiplied by this when set. `None` = anchoring off.
    pub anchor_ratio: Option<f64>,
    /// Lower clamp on every returned vol.
    pub floor_vol: f64,
    /// Upper clamp on every returned vol.
    pub cap_vol: f64,
}

/// A frozen surface: the blended base ATM sigma plus the shape parameters it
/// was built with. Rebuild via [`VolSurface::from_windows`] whenever the
/// realized-vol windows update.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolSurface {
    /// Blended base ATM sigma, already including risk premium, anchor ratio,
    /// and the [floor, cap] clamp.
    sigma_atm: f64,
    /// True when every window was cold and `fallback_vol` was used.
    fallback: bool,
    params: SurfaceParams,
}

impl VolSurface {
    /// Build the surface from realized-vol windows.
    ///
    /// Base sigma = `max(weighted_mean, 0.8·max_window)` over the live
    /// windows (see module docs for why the max-leaning blend), where "live"
    /// means `annualized_vol` is `Some`, finite, positive, and `weight > 0`.
    /// When no window is live the surface uses `fallback_vol` and reports it
    /// via [`is_fallback`](Self::is_fallback) so callers can widen quotes or
    /// refuse size while cold. The result then gets `+ risk_premium`,
    /// `× anchor_ratio` (when set), and the [floor, cap] clamp, in that
    /// order.
    pub fn from_windows(
        samples: &[WindowSample],
        fallback_vol: f64,
        params: &SurfaceParams,
    ) -> VolSurface {
        let mut weight_sum = 0.0;
        let mut weighted_vol_sum = 0.0;
        let mut max_vol = f64::NEG_INFINITY;
        for s in samples {
            if s.weight <= 0.0 {
                continue;
            }
            if let Some(v) = s.annualized_vol {
                if v.is_finite() && v > 0.0 {
                    weight_sum += s.weight;
                    weighted_vol_sum += s.weight * v;
                    max_vol = max_vol.max(v);
                }
            }
        }

        let (base, fallback) = if weight_sum > 0.0 {
            let mean = weighted_vol_sum / weight_sum;
            (mean.max(0.8 * max_vol), false)
        } else {
            (fallback_vol, true)
        };

        let mut sigma = base + params.risk_premium;
        if let Some(ratio) = params.anchor_ratio {
            sigma *= ratio;
        }
        VolSurface {
            sigma_atm: sigma.clamp(params.floor_vol, params.cap_vol),
            fallback,
            params: *params,
        }
    }

    /// True when the surface was built with no live window and is running on
    /// the configured fallback vol.
    pub fn is_fallback(&self) -> bool {
        self.fallback
    }

    /// Short-tenor boost multiplier: `1 + boost·exp(−τ/decay)`. Disabled
    /// (returns 1.0) when the boost is zero or the decay is non-positive;
    /// negative τ is treated as 0 (maximum boost).
    fn term(&self, t_years: f64) -> f64 {
        let p = &self.params;
        if p.term_short_boost == 0.0 || p.term_decay_years <= 0.0 {
            return 1.0;
        }
        1.0 + p.term_short_boost * (-t_years.max(0.0) / p.term_decay_years).exp()
    }

    /// ATM vol at tenor `t_years`: `σ_atm · term(τ)`, clamped [floor, cap].
    pub fn atm(&self, t_years: f64) -> f64 {
        (self.sigma_atm * self.term(t_years)).clamp(self.params.floor_vol, self.params.cap_vol)
    }

    /// Vol at (strike, tenor): the ATM term-structure vol shaped by the
    /// smile prior (see module docs). Degenerate inputs — `t_years <= 0`,
    /// non-positive spot/strike, or a zero z-denominator — return the
    /// clamped ATM vol instead of NaN.
    pub fn vol(&self, spot: f64, strike: f64, t_years: f64) -> f64 {
        if t_years <= 0.0 || spot <= 0.0 || strike <= 0.0 {
            return self.atm(t_years);
        }
        let denom = self.sigma_atm * t_years.sqrt();
        if denom <= 0.0 {
            return self.atm(t_years);
        }
        let p = &self.params;
        let z = (strike / spot).ln() / denom;
        let mult = (1.0 + p.skew * z + p.convexity * z * z).clamp(0.25, 4.0);
        (self.sigma_atm * self.term(t_years) * mult).clamp(p.floor_vol, p.cap_vol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} vs {b}, eps {eps}");
    }

    /// Shape-neutral params: no premium, no smile, no term boost, wide clamp.
    fn flat_params() -> SurfaceParams {
        SurfaceParams {
            risk_premium: 0.0,
            skew: 0.0,
            convexity: 0.0,
            term_short_boost: 0.0,
            term_decay_years: 0.0,
            anchor_ratio: None,
            floor_vol: 0.05,
            cap_vol: 3.0,
        }
    }

    fn live(v: f64, w: f64) -> WindowSample {
        WindowSample { annualized_vol: Some(v), weight: w }
    }

    #[test]
    fn weighted_mean_when_no_spike() {
        // 0.4·1 + 0.5·2 + 0.6·1 over weight 4 → mean 0.5; 0.8·max = 0.48,
        // so the mean wins.
        let s = VolSurface::from_windows(
            &[live(0.4, 1.0), live(0.5, 2.0), live(0.6, 1.0)],
            0.9,
            &flat_params(),
        );
        assert!(!s.is_fallback());
        close(s.atm(1.0), 0.5, 1e-12);
    }

    #[test]
    fn single_window_spike_lifts_the_surface() {
        // Mean = (0.3 + 0.3 + 1.5)/3 = 0.7, but 0.8·1.5 = 1.2 wins: the 1d
        // spike lifts the surface past what the 7d/30d average would say.
        let s = VolSurface::from_windows(
            &[live(0.3, 1.0), live(0.3, 1.0), live(1.5, 1.0)],
            0.9,
            &flat_params(),
        );
        close(s.atm(1.0), 1.2, 1e-12);
    }

    #[test]
    fn all_cold_uses_fallback_and_reports_it() {
        let cold = WindowSample { annualized_vol: None, weight: 1.0 };
        let s = VolSurface::from_windows(&[cold, cold], 0.65, &flat_params());
        assert!(s.is_fallback());
        close(s.atm(1.0), 0.65, 1e-12);
        // Zero-weight and non-finite windows count as cold too.
        let s = VolSurface::from_windows(
            &[live(0.5, 0.0), live(f64::NAN, 1.0), live(-0.2, 1.0)],
            0.65,
            &flat_params(),
        );
        assert!(s.is_fallback());
        close(s.atm(1.0), 0.65, 1e-12);
    }

    #[test]
    fn risk_premium_is_additive_then_anchor_multiplies() {
        let mut p = flat_params();
        p.risk_premium = 0.05;
        let s = VolSurface::from_windows(&[live(0.5, 1.0)], 0.9, &p);
        close(s.atm(1.0), 0.55, 1e-12);
        p.anchor_ratio = Some(1.2);
        let s = VolSurface::from_windows(&[live(0.5, 1.0)], 0.9, &p);
        close(s.atm(1.0), 0.55 * 1.2, 1e-12);
    }

    #[test]
    fn base_sigma_is_clamped_to_floor_and_cap() {
        let mut p = flat_params();
        p.risk_premium = 1.5;
        let s = VolSurface::from_windows(&[live(2.0, 1.0)], 0.9, &p);
        close(s.atm(1.0), 3.0, 1e-12); // 3.5 capped
        let s = VolSurface::from_windows(&[live(0.01, 1.0)], 0.9, &flat_params());
        close(s.atm(1.0), 0.05, 1e-12); // floored
    }

    #[test]
    fn term_boost_decays_with_tenor() {
        let mut p = flat_params();
        p.term_short_boost = 0.5;
        p.term_decay_years = 0.02; // ~1 week e-folding
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        close(s.atm(0.0), 0.6 * 1.5, 1e-12);
        close(s.atm(0.02), 0.6 * (1.0 + 0.5 / std::f64::consts::E), 1e-12);
        // Monotone decreasing toward the flat far tenor.
        assert!(s.atm(0.0) > s.atm(0.01));
        assert!(s.atm(0.01) > s.atm(0.1));
        close(s.atm(1.0), 0.6, 1e-6); // e^{-50} ≈ 0
    }

    #[test]
    fn positive_skew_tilts_wings_like_smile() {
        let mut p = flat_params();
        p.skew = 0.1;
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        let t = 7.0 / 365.0;
        assert!(s.vol(100.0, 130.0, t) > s.atm(t), "upside wing should be rich");
        assert!(s.vol(100.0, 80.0, t) < s.atm(t), "downside wing should be cheap");
        // Monotone in strike for a pure linear skew (within the clamp band).
        let vols: Vec<f64> = [90.0, 100.0, 110.0, 120.0]
            .iter()
            .map(|k| s.vol(100.0, *k, t))
            .collect();
        for w in vols.windows(2) {
            assert!(w[1] > w[0], "not increasing: {vols:?}");
        }
    }

    #[test]
    fn convexity_raises_both_wings() {
        let mut p = flat_params();
        p.convexity = 0.05;
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        let t = 7.0 / 365.0;
        assert!(s.vol(100.0, 130.0, t) > s.atm(t));
        assert!(s.vol(100.0, 80.0, t) > s.atm(t));
        close(s.vol(100.0, 100.0, t), s.atm(t), 1e-12); // z = 0 at ATM
    }

    #[test]
    fn smile_multiplier_is_clamped_and_result_respects_cap() {
        let mut p = flat_params();
        p.skew = 10.0; // absurd wing
        p.cap_vol = 10.0; // cap out of the way: see the raw 4.0 mult clamp
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        close(s.vol(100.0, 200.0, 7.0 / 365.0), 0.6 * 4.0, 1e-12);
        p.skew = -10.0;
        p.floor_vol = 0.01;
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        close(s.vol(100.0, 200.0, 7.0 / 365.0), 0.6 * 0.25, 1e-12);
        // With a tight cap the final clamp wins over the smile.
        p.skew = 10.0;
        p.cap_vol = 1.0;
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        close(s.vol(100.0, 200.0, 7.0 / 365.0), 1.0, 1e-12);
    }

    #[test]
    fn degenerate_inputs_return_clamped_atm() {
        let mut p = flat_params();
        p.skew = 0.2;
        let s = VolSurface::from_windows(&[live(0.6, 1.0)], 0.9, &p);
        close(s.vol(100.0, 130.0, 0.0), s.atm(0.0), 1e-12);
        close(s.vol(100.0, 130.0, -1.0), s.atm(-1.0), 1e-12);
        close(s.vol(0.0, 130.0, 0.5), s.atm(0.5), 1e-12);
        close(s.vol(100.0, 0.0, 0.5), s.atm(0.5), 1e-12);
    }
}
