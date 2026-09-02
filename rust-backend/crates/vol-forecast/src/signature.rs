//! Volatility signature: realized vol as a function of sampling interval.
//! Microstructure noise inflates RV at short intervals (doc 07 §4: SUI's
//! 1-minute RV is ~45% above its 1-hour RV, BTC is flat), so the sampling
//! interval an asset is priced off is *derived* as the first candidate
//! whose RV is within a tolerance of the reference (1-hour) value.

use crate::rv::{resample, MS_PER_YEAR};

/// RV at one candidate interval.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SignaturePoint {
    pub interval_ms: u64,
    /// Annualized realized vol over the signature window; 0 when the
    /// interval had no valid returns.
    pub annualized_vol: f64,
    /// Valid grid returns the estimate used.
    pub n_returns: usize,
}

/// RV over `[end_ms − span_ms, end_ms]` at each candidate interval.
pub fn volatility_signature(
    history: &[(u64, f64)],
    end_ms: u64,
    span_ms: u64,
    candidates: &[u64],
) -> Vec<SignaturePoint> {
    candidates
        .iter()
        .map(|&interval_ms| {
            let grid = resample(history, end_ms, span_ms, interval_ms);
            let (r, valid) = grid.returns();
            let mut sum_sq = 0.0;
            let mut n = 0usize;
            for i in 0..r.len() {
                if valid[i] {
                    sum_sq += r[i] * r[i];
                    n += 1;
                }
            }
            let annualized_vol = if n > 0 {
                (sum_sq / (n as f64 * interval_ms as f64 / MS_PER_YEAR)).sqrt()
            } else {
                0.0
            };
            SignaturePoint {
                interval_ms,
                annualized_vol,
                n_returns: n,
            }
        })
        .collect()
}

/// Median spacing between consecutive raw observations (ms); 0 when
/// fewer than two observations.
pub fn raw_cadence_ms(history: &[(u64, f64)]) -> u64 {
    if history.len() < 2 {
        return 0;
    }
    let mut d: Vec<u64> = history
        .windows(2)
        .map(|w| w[1].0.saturating_sub(w[0].0))
        .collect();
    d.sort_unstable();
    d[d.len() / 2]
}

/// Interval selection rule. Candidates are eligible when they are at least
/// ~the raw cadence (nothing below the data's own resolution) and carry
/// `min_returns` valid returns. The reference is the eligible point at
/// `reference_ms` (else the largest eligible interval at or below it, else
/// the smallest eligible). The result is the smallest eligible interval
/// whose vol is within `tolerance` (relative) of the reference; `None` when
/// nothing is eligible.
pub fn derive_interval(
    points: &[SignaturePoint],
    raw_cadence_ms: u64,
    reference_ms: u64,
    tolerance: f64,
    min_returns: usize,
) -> Option<u64> {
    let eligible: Vec<&SignaturePoint> = points
        .iter()
        .filter(|p| {
            p.annualized_vol > 0.0
                && p.n_returns >= min_returns
                && (p.interval_ms as f64) >= 0.9 * raw_cadence_ms as f64
        })
        .collect();
    if eligible.is_empty() {
        return None;
    }
    let reference = eligible
        .iter()
        .find(|p| p.interval_ms == reference_ms)
        .or_else(|| {
            eligible
                .iter()
                .filter(|p| p.interval_ms <= reference_ms)
                .max_by_key(|p| p.interval_ms)
        })
        .or_else(|| eligible.iter().min_by_key(|p| p.interval_ms))?;
    let ref_vol = reference.annualized_vol;
    let mut sorted = eligible.clone();
    sorted.sort_by_key(|p| p.interval_ms);
    sorted
        .iter()
        .find(|p| (p.annualized_vol / ref_vol - 1.0).abs() <= tolerance)
        .map(|p| p.interval_ms)
        .or(Some(reference.interval_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(interval_ms: u64, vol: f64) -> SignaturePoint {
        SignaturePoint {
            interval_ms,
            annualized_vol: vol,
            n_returns: 1_000,
        }
    }

    #[test]
    fn doc07_sui_table_picks_15m_and_btc_picks_1m() {
        // doc 07 §4 signature table (1m / 15m / 1h / 4h / 1d).
        let sui = [
            pt(60_000, 1.270),
            pt(900_000, 0.916),
            pt(3_600_000, 0.857),
            pt(14_400_000, 0.862),
            pt(86_400_000, 0.870),
        ];
        assert_eq!(
            derive_interval(&sui, 60_000, 3_600_000, 0.08, 100),
            Some(900_000)
        );
        let btc = [
            pt(60_000, 0.459),
            pt(900_000, 0.443),
            pt(3_600_000, 0.429),
            pt(14_400_000, 0.418),
            pt(86_400_000, 0.430),
        ];
        assert_eq!(
            derive_interval(&btc, 60_000, 3_600_000, 0.08, 100),
            Some(60_000)
        );
    }

    #[test]
    fn candidates_below_the_raw_cadence_are_ineligible() {
        let sui = [
            pt(60_000, 1.270),
            pt(300_000, 0.99),
            pt(900_000, 0.916),
            pt(3_600_000, 0.857),
        ];
        // Live sampler at 5m (with jitter): 1m is off the table, 5m is not.
        assert_eq!(
            derive_interval(&sui, 300_010, 3_600_000, 0.20, 100),
            Some(300_000)
        );
        // Nothing eligible → None.
        assert_eq!(derive_interval(&sui, 7_200_000, 3_600_000, 0.08, 100), None);
        let thin = [SignaturePoint {
            interval_ms: 60_000,
            annualized_vol: 1.0,
            n_returns: 5,
        }];
        assert_eq!(derive_interval(&thin, 0, 3_600_000, 0.08, 100), None);
    }

    #[test]
    fn raw_cadence_is_the_median_spacing() {
        let h = [
            (0u64, 1.0),
            (300_000, 1.0),
            (600_000, 1.0),
            (1_800_000, 1.0),
            (2_100_000, 1.0),
        ];
        assert_eq!(raw_cadence_ms(&h), 300_000);
        assert_eq!(raw_cadence_ms(&h[..1]), 0);
    }
}
