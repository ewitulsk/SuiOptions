//! Per-round records → summary statistics (doc 06 §9.1). One `Summary`
//! per scenario cell; the backtester's sweep runner emits one row each.

use serde::{Deserialize, Serialize};

use crate::types::{RoundRecord, PPS_SCALE};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub rounds: usize,
    /// Full-sample geometric APY, underlying-denominated (pps growth).
    pub apy_underlying: f64,
    /// Full-sample geometric APY, USD-denominated (pps × spot growth).
    pub apy_usd: f64,
    /// Ribbon-style trailing-4-round annualization, underlying terms.
    pub apy_ribbon_4r: f64,
    /// Premium yield per round (premium / round-start AUM, underlying
    /// terms): p5 / p50 / p95.
    pub premium_yield_p5: f64,
    pub premium_yield_p50: f64,
    pub premium_yield_p95: f64,
    /// Fraction of selling rounds that ended with any exercise (target ≈
    /// N(d2) ≈ 8–9% at 0.10Δ).
    pub callaway_freq: f64,
    /// Max drawdown of the USD share value (pps × spot).
    pub max_drawdown_usd: f64,
    /// Annualized Sharpe / Sortino of per-round USD returns.
    pub sharpe: f64,
    pub sortino: f64,
    /// Expected shortfall: mean of the worst 5% per-round USD returns.
    pub es95: f64,
    /// Total USD growth of the strategy vs holding the underlying:
    /// (1 + strategy) / (1 + hodl) − 1.
    pub vs_hodl: f64,
    /// Σ unsold / Σ offered across selling rounds.
    pub unsold_rate: f64,
    /// Total fees charged over the sample, underlying units.
    pub total_fees: u64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn annualize(total_growth: f64, rounds: usize, rounds_per_year: f64) -> f64 {
    if rounds == 0 || total_growth <= 0.0 {
        return 0.0;
    }
    total_growth.powf(rounds_per_year / rounds as f64) - 1.0
}

pub fn summarize(records: &[RoundRecord], rounds_per_year: f64) -> Summary {
    let n = records.len();
    if n == 0 {
        return Summary {
            rounds: 0,
            apy_underlying: 0.0,
            apy_usd: 0.0,
            apy_ribbon_4r: 0.0,
            premium_yield_p5: 0.0,
            premium_yield_p50: 0.0,
            premium_yield_p95: 0.0,
            callaway_freq: 0.0,
            max_drawdown_usd: 0.0,
            sharpe: 0.0,
            sortino: 0.0,
            es95: 0.0,
            vs_hodl: 0.0,
            unsold_rate: 0.0,
            total_fees: 0,
        };
    }

    // Per-round returns. pps before the first record is genesis 1.0.
    let mut prev_pps = PPS_SCALE as f64;
    let mut r_underlying = Vec::with_capacity(n);
    let mut r_usd = Vec::with_capacity(n);
    for rec in records {
        let pps = rec.pps.0 as f64;
        r_underlying.push(pps / prev_pps - 1.0);
        // USD share value: pps × spot. Round i runs spot_0 → spot_t.
        r_usd.push((pps * rec.spot_t) / (prev_pps * rec.spot_0) - 1.0);
        prev_pps = pps;
    }

    let growth_u: f64 = r_underlying.iter().map(|r| 1.0 + r).product();
    let growth_usd: f64 = r_usd.iter().map(|r| 1.0 + r).product();
    let hodl_growth: f64 = records.iter().map(|r| r.spot_t / r.spot_0).product();

    let last4: f64 = r_underlying.iter().rev().take(4).map(|r| 1.0 + r).product();
    let apy_ribbon_4r = annualize(last4, r_underlying.len().min(4), rounds_per_year);

    // Premium yield per selling round, in underlying terms.
    let mut yields: Vec<f64> = records
        .iter()
        .filter(|r| r.written.get() > 0)
        .map(|r| (r.premium.get() as f64 / r.spot_0) / r.aum.get().max(1) as f64)
        .collect();
    yields.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let selling_rounds = records.iter().filter(|r| r.written.get() > 0).count();
    let callaway = records.iter().filter(|r| r.phi > 0.0).count();

    // Drawdown on the USD share-value curve.
    let mut peak = f64::MIN;
    let mut max_dd = 0.0f64;
    let mut value = 1.0;
    for r in &r_usd {
        value *= 1.0 + r;
        peak = peak.max(value);
        max_dd = max_dd.max(1.0 - value / peak);
    }

    let mean_usd = r_usd.iter().sum::<f64>() / n as f64;
    let var_usd =
        r_usd.iter().map(|r| (r - mean_usd).powi(2)).sum::<f64>() / (n as f64 - 1.0).max(1.0);
    let downside: Vec<f64> = r_usd.iter().filter(|r| **r < 0.0).map(|r| r * r).collect();
    let downside_dev = if downside.is_empty() {
        0.0
    } else {
        (downside.iter().sum::<f64>() / n as f64).sqrt()
    };
    let sharpe = if var_usd > 0.0 {
        mean_usd / var_usd.sqrt() * rounds_per_year.sqrt()
    } else {
        0.0
    };
    let sortino = if downside_dev > 0.0 {
        mean_usd / downside_dev * rounds_per_year.sqrt()
    } else {
        0.0
    };

    let mut sorted_usd = r_usd.clone();
    sorted_usd.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let tail = ((n as f64 * 0.05).ceil() as usize).max(1);
    let es95 = sorted_usd[..tail].iter().sum::<f64>() / tail as f64;

    let offered: u64 = records.iter().map(|r| r.written.get() + r.unsold.get()).sum();
    let unsold: u64 = records.iter().map(|r| r.unsold.get()).sum();

    Summary {
        rounds: n,
        apy_underlying: annualize(growth_u, n, rounds_per_year),
        apy_usd: annualize(growth_usd, n, rounds_per_year),
        apy_ribbon_4r,
        premium_yield_p5: percentile(&yields, 0.05),
        premium_yield_p50: percentile(&yields, 0.50),
        premium_yield_p95: percentile(&yields, 0.95),
        callaway_freq: if selling_rounds > 0 {
            callaway as f64 / selling_rounds as f64
        } else {
            0.0
        },
        max_drawdown_usd: max_dd,
        sharpe,
        sortino,
        es95,
        vs_hodl: (1.0 + (growth_usd - 1.0)) / hodl_growth - 1.0,
        unsold_rate: if offered > 0 { unsold as f64 / offered as f64 } else { 0.0 },
        total_fees: records.iter().map(|r| r.mgmt_fee.get() + r.perf_fee.get()).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Pps, SettleAmt, ShareAmt, UnderlyingAmt};

    fn record(round: u64, pps: u128, spot_0: f64, spot_t: f64, phi: f64) -> RoundRecord {
        RoundRecord {
            round,
            spot_0,
            spot_t,
            strike: spot_0 * 1.1,
            sigma_iv: 0.6,
            premium: SettleAmt(1_000),
            phi,
            written: UnderlyingAmt(1_000_000),
            unsold: UnderlyingAmt::ZERO,
            mgmt_fee: UnderlyingAmt(10),
            perf_fee: UnderlyingAmt(5),
            pps: Pps(pps),
            aum: UnderlyingAmt(1_000_000),
            shares: ShareAmt(1_000_000),
        }
    }

    #[test]
    fn flat_rounds_summarize_to_zero_apy() {
        let recs: Vec<_> =
            (0..10).map(|i| record(i + 1, PPS_SCALE, 3.5e-3, 3.5e-3, 0.0)).collect();
        let s = summarize(&recs, 52.0);
        assert_eq!(s.rounds, 10);
        assert!(s.apy_underlying.abs() < 1e-12);
        assert!(s.apy_usd.abs() < 1e-12);
        assert!(s.max_drawdown_usd.abs() < 1e-12);
        assert_eq!(s.callaway_freq, 0.0);
        assert_eq!(s.total_fees, 150);
    }

    #[test]
    fn steady_growth_annualizes_geometrically() {
        // +1% per round for 52 rounds/yr ⇒ APY = 1.01^52 − 1.
        let mut pps = PPS_SCALE as f64;
        let recs: Vec<_> = (0..26)
            .map(|i| {
                pps *= 1.01;
                record(i + 1, pps as u128, 3.5e-3, 3.5e-3, 0.0)
            })
            .collect();
        let s = summarize(&recs, 52.0);
        let expected = 1.01f64.powi(52) - 1.0;
        assert!((s.apy_underlying - expected).abs() < 1e-3, "{}", s.apy_underlying);
        // Flat spot ⇒ USD APY equals underlying APY.
        assert!((s.apy_usd - s.apy_underlying).abs() < 1e-9);
        // vs HODL: spot flat, strategy grows ⇒ outperformance.
        assert!(s.vs_hodl > 0.0);
    }

    #[test]
    fn callaway_and_drawdown_track_inputs() {
        let recs = vec![
            record(1, PPS_SCALE, 3.5e-3, 3.5e-3, 0.0),
            // Spot −20%: USD value drops even with flat pps.
            record(2, PPS_SCALE, 3.5e-3, 2.8e-3, 0.0),
            record(3, PPS_SCALE, 2.8e-3, 2.8e-3, 1.0),
        ];
        let s = summarize(&recs, 52.0);
        assert!((s.callaway_freq - 1.0 / 3.0).abs() < 1e-12);
        assert!((s.max_drawdown_usd - 0.2).abs() < 1e-9);
        assert!(s.es95 < 0.0, "worst tail must be negative");
    }
}
