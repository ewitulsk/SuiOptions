//! Pure predicted-APY math. No I/O — fed resolved market data + vault state.
//!
//! Tier 1 annualizes the premium the vault is on track to collect this round
//! (from live RFQ bids). Tier 2 prices the next K rounds' premium with
//! Black–Scholes at the keeper's delta-target strike, net of fees.
//!
//! Both are *premium-yield* APYs: they assume the calls expire unassigned and
//! swaps fill at quote. Realized (PPS-based) APY additionally nets assignment
//! drag and swap slippage, so in a sharp rally realized comes in below these.

use pricing::{call_price_per_unit, CallInputs};

const YEAR_MS: f64 = 365.25 * 86_400_000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct PredictionPoint {
    /// `current` (Tier 1) | `forecast` (Tier 2).
    pub kind: &'static str,
    /// 0 = current round; 1..=K = rounds ahead.
    pub horizon: i32,
    pub t_ms: i64,
    pub apy: f64,
    pub confidence: f64,
}

/// Resolved inputs for one vault at one tick.
pub struct VaultInputs {
    /// USD cross: settlement units per 1 underlying.
    pub spot: f64,
    /// Annualized realized vol of the underlying.
    pub sigma: f64,
    /// Assets under management, in whole underlying units.
    pub aum_underlying: f64,
    /// Round length in ms (derived from finalize spacing, else config).
    pub round_ms: u64,
    /// Expiry of the current round (ms) — where the Tier-1 point lands.
    pub current_expiry_ms: i64,
    /// Premium the vault is on track to collect this round, in whole
    /// underlying units (Tier 1).
    pub current_premium_underlying: f64,
    /// Fraction of the round's slice notional already settled (Tier-1
    /// confidence: rises toward 1.0 as auctions close).
    pub current_premium_confidence: f64,
    /// Performance fee as a fraction (e.g. 0.10).
    pub perf_fee: f64,
    /// Management fee, annualized fraction (e.g. 0.02).
    pub mgmt_fee_annual: f64,
    /// Tier-2 horizon (number of future rounds).
    pub horizon: u32,
    /// Keeper's delta-target snap (e.g. 0.10).
    pub delta_target: f64,
}

/// Annualize a per-round yield: `(1 + y)^(year / round) − 1`. Returns `None`
/// if the result isn't finite (e.g. `y ≤ −1`).
fn annualize(round_yield: f64, round_ms: u64) -> Option<f64> {
    if round_ms == 0 || 1.0 + round_yield <= 0.0 {
        return None;
    }
    let periods = YEAR_MS / round_ms as f64;
    let apy = (1.0 + round_yield).powf(periods) - 1.0;
    apy.is_finite().then_some(apy)
}

pub fn predict(i: &VaultInputs) -> Vec<PredictionPoint> {
    let mut out = Vec::new();

    // ── Tier 1: current round, from live RFQ premium ──
    if i.aum_underlying > 0.0 && i.current_premium_underlying >= 0.0 {
        let round_yield = i.current_premium_underlying / i.aum_underlying;
        if let Some(apy) = annualize(round_yield, i.round_ms) {
            out.push(PredictionPoint {
                kind: "current",
                horizon: 0,
                t_ms: i.current_expiry_ms,
                apy,
                confidence: i.current_premium_confidence.clamp(0.0, 1.0),
            });
        }
    }

    // ── Tier 2: forecast next K rounds via Black–Scholes ──
    if i.spot > 0.0 && i.sigma > 0.0 {
        let t_years = i.round_ms as f64 / YEAR_MS;
        let strike = pricing::strike_for_delta(i.spot, i.sigma, t_years, 0.0, i.delta_target);
        let call_px = call_price_per_unit(CallInputs {
            spot: i.spot,
            strike,
            t_years,
            r: 0.0,
            sigma: i.sigma,
        });
        let mgmt_round = i.mgmt_fee_annual * t_years;
        // Premium as a fraction of underlying notional, net of fees.
        let round_yield = (call_px / i.spot) * (1.0 - i.perf_fee) - mgmt_round;
        if let Some(apy) = annualize(round_yield, i.round_ms) {
            for n in 1..=i.horizon as i64 {
                out.push(PredictionPoint {
                    // Stationary spot/vol assumption → flat forecast; the
                    // horizon extends the line, confidence decays with it.
                    kind: "forecast",
                    horizon: n as i32,
                    t_ms: i.current_expiry_ms + n * i.round_ms as i64,
                    apy,
                    confidence: 0.6_f64.powi(n as i32),
                });
            }
        }
    }

    out
}

/// Median finalize-to-finalize spacing across a vault's finalized rounds, or
/// `None` with fewer than two. Used to derive `round_ms` from observed cadence.
pub fn median_round_ms(mut finalize_ms: Vec<u64>) -> Option<u64> {
    finalize_ms.sort_unstable();
    if finalize_ms.len() < 2 {
        return None;
    }
    let mut diffs: Vec<u64> = finalize_ms.windows(2).map(|w| w[1] - w[0]).collect();
    diffs.sort_unstable();
    Some(diffs[diffs.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier1_annualizes_weekly_premium() {
        const WEEK: u64 = 7 * 86_400_000;
        let i = VaultInputs {
            spot: 50_000.0,
            sigma: 0.0, // disable Tier 2 to isolate Tier 1
            aum_underlying: 100.0,
            round_ms: WEEK,
            current_expiry_ms: 1_000,
            current_premium_underlying: 0.2, // 0.2% of AUM this week
            current_premium_confidence: 0.5,
            perf_fee: 0.10,
            mgmt_fee_annual: 0.02,
            horizon: 4,
            delta_target: 0.10,
        };
        let pts = predict(&i);
        let cur = pts.iter().find(|p| p.kind == "current").unwrap();
        // (1 + 0.002)^(365.25/7) − 1 ≈ 11%.
        assert!((cur.apy - 0.110).abs() < 0.01, "apy {}", cur.apy);
        assert!(pts.iter().all(|p| p.kind != "forecast"));
    }

    #[test]
    fn tier2_emits_horizon_points() {
        const WEEK: u64 = 7 * 86_400_000;
        let i = VaultInputs {
            spot: 50_000.0,
            sigma: 0.6,
            aum_underlying: 0.0, // disable Tier 1
            round_ms: WEEK,
            current_expiry_ms: 1_000,
            current_premium_underlying: 0.0,
            current_premium_confidence: 0.0,
            perf_fee: 0.10,
            mgmt_fee_annual: 0.02,
            horizon: 4,
            delta_target: 0.10,
        };
        let pts = predict(&i);
        let fc: Vec<_> = pts.iter().filter(|p| p.kind == "forecast").collect();
        assert_eq!(fc.len(), 4);
        assert_eq!(fc[0].t_ms, 1_000 + WEEK as i64);
        assert!(fc[0].apy.is_finite());
        assert!(fc[1].confidence < fc[0].confidence);
    }

    #[test]
    fn median_round_ms_from_finalizes() {
        const WEEK: u64 = 7 * 86_400_000;
        assert_eq!(median_round_ms(vec![]), None);
        assert_eq!(median_round_ms(vec![0]), None);
        assert_eq!(median_round_ms(vec![0, WEEK, 2 * WEEK]), Some(WEEK));
    }
}
