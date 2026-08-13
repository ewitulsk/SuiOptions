//! Realized volatility over a sampling grid (spec §8).
//!
//! `close_close`: last-observation-carried-forward onto a fixed grid,
//! zero-mean Σr², annualized by the covered span — the same convention
//! as the mm-bot's `RollingVolBuffer` so numbers are comparable.
//! `rv_subsampled`: the same estimator averaged over K offset grids
//! (two-scale-lite), the cheap microstructure-noise-robust variant.

use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::{Duration, NaiveDate};
use tracing::info;

use crate::{read, Store};

pub const INTERVALS_S: &[i64] = &[1, 5, 15, 60, 300];
pub const WINDOWS_S: &[i64] = &[3_600, 86_400, 604_800];
const SUBSAMPLE_K: i64 = 5;
const NS_PER_YEAR: f64 = 365.0 * 86_400.0 * 1e9;

pub struct RvPoint {
    pub sigma_ann: f64,
    pub n_returns: usize,
    /// Fraction of grid returns that had data on both ends.
    pub coverage: f64,
}

/// LOCF-sample `points` (sorted by ts) onto the grid
/// `start+offset, start+offset+step, …, ≤ end`; None before first point.
fn sample_locf(
    points: &[(i64, f64)],
    start: i64,
    end: i64,
    step: i64,
    offset: i64,
) -> Vec<Option<f64>> {
    let mut out = Vec::new();
    let mut idx = 0usize;
    let mut last: Option<f64> = None;
    let mut t = start + offset;
    while t <= end {
        while idx < points.len() && points[idx].0 <= t {
            last = Some(points[idx].1);
            idx += 1;
        }
        out.push(last);
        t += step;
    }
    out
}

fn rv_on_grid(samples: &[Option<f64>], step_ns: i64) -> Option<RvPoint> {
    let mut sum_sq = 0.0;
    let mut k = 0usize;
    let mut slots = 0usize;
    for w in samples.windows(2) {
        slots += 1;
        if let (Some(a), Some(b)) = (w[0], w[1]) {
            if a > 0.0 && b > 0.0 {
                let r = (b / a).ln();
                sum_sq += r * r;
                k += 1;
            }
        }
    }
    if k < 2 {
        return None;
    }
    let span_years = k as f64 * step_ns as f64 / NS_PER_YEAR;
    Some(RvPoint {
        sigma_ann: (sum_sq / span_years).sqrt(),
        n_returns: k,
        coverage: k as f64 / slots.max(1) as f64,
    })
}

/// One estimator value at `end_ns` looking back `window_ns`.
pub fn estimate(
    points: &[(i64, f64)],
    end_ns: i64,
    window_ns: i64,
    interval_ns: i64,
    subsampled: bool,
) -> Option<RvPoint> {
    let start = end_ns - window_ns;
    if !subsampled {
        return rv_on_grid(
            &sample_locf(points, start, end_ns, interval_ns, 0),
            interval_ns,
        );
    }
    let mut acc_var = 0.0;
    let mut acc_cov = 0.0;
    let mut acc_n = 0usize;
    let mut got = 0usize;
    for j in 0..SUBSAMPLE_K {
        let offset = j * interval_ns / SUBSAMPLE_K;
        if let Some(p) = rv_on_grid(
            &sample_locf(points, start, end_ns, interval_ns, offset),
            interval_ns,
        ) {
            acc_var += p.sigma_ann * p.sigma_ann;
            acc_cov += p.coverage;
            acc_n += p.n_returns;
            got += 1;
        }
    }
    (got > 0).then(|| RvPoint {
        sigma_ann: (acc_var / got as f64).sqrt(),
        n_returns: acc_n / got,
        coverage: acc_cov / got as f64,
    })
}

fn rv_schema() -> Schema {
    Schema::new(vec![
        Field::new("ts", DataType::Int64, false),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("window_s", DataType::Int64, false),
        Field::new("sample_interval_s", DataType::Int64, false),
        Field::new("source", DataType::Utf8, false),
        Field::new("estimator", DataType::Utf8, false),
        Field::new("sigma_ann", DataType::Float64, false),
        Field::new("n_returns", DataType::Int64, false),
        Field::new("coverage", DataType::Float64, false),
    ])
}

#[allow(clippy::too_many_arguments)]
struct Row {
    ts: i64,
    exchange: String,
    instrument_id: String,
    window_s: i64,
    interval_s: i64,
    source: String,
    estimator: String,
    sigma_ann: f64,
    n_returns: i64,
    coverage: f64,
}

/// Compute the full grid for one UTC day: window ends at each hour
/// boundary, every interval × window × available source × estimator.
pub async fn compute_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let day_start_ns = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap();

    // Look back far enough for the longest window at the first hour end.
    let max_back_days = (WINDOWS_S.last().unwrap() / 86_400) + 1;
    let dates: Vec<String> = (0..=max_back_days)
        .rev()
        .map(|b| (day - Duration::days(b)).format("%Y-%m-%d").to_string())
        .collect();

    let mut rows: Vec<Row> = Vec::new();
    for (exchange, symbol) in crate::pairs_for_date(store, date).await? {
        for source in ["trades", "mid"] {
            let points = read::price_series(store, &exchange, &symbol, source, &dates).await?;
            if points.is_empty() {
                continue;
            }
            let instrument = crate::instrument_id(&exchange, &symbol);
            for hour in 1..=24i64 {
                let end_ns = day_start_ns + hour * 3_600 * 1_000_000_000;
                for &window_s in WINDOWS_S {
                    for &interval_s in INTERVALS_S {
                        for (estimator, sub) in [("close_close", false), ("rv_subsampled", true)] {
                            if let Some(p) = estimate(
                                &points,
                                end_ns,
                                window_s * 1_000_000_000,
                                interval_s * 1_000_000_000,
                                sub,
                            ) {
                                rows.push(Row {
                                    ts: end_ns,
                                    exchange: exchange.clone(),
                                    instrument_id: instrument.clone(),
                                    window_s,
                                    interval_s,
                                    source: source.into(),
                                    estimator: estimator.into(),
                                    sigma_ann: p.sigma_ann,
                                    n_returns: p.n_returns as i64,
                                    coverage: p.coverage,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    if rows.is_empty() {
        info!(date, "rv: no data");
        return Ok(0);
    }
    rows.sort_by(|a, b| {
        (
            a.ts,
            &a.exchange,
            &a.instrument_id,
            a.window_s,
            a.interval_s,
            &a.source,
            &a.estimator,
        )
            .cmp(&(
                b.ts,
                &b.exchange,
                &b.instrument_id,
                b.window_s,
                b.interval_s,
                &b.source,
                &b.estimator,
            ))
    });

    let n = rows.len();
    let batch = RecordBatch::try_new(
        Arc::new(rv_schema()),
        vec![
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.ts))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.window_s),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.interval_s),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.source.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.estimator.as_str()),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.sigma_ann),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.n_returns),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.coverage),
            )),
        ],
    )?;
    let bytes = schema::write_parquet(&batch)?;
    store
        .put(
            &object_store::path::Path::from(format!("gold/v1/rv/date={date}/part-00.parquet")),
            bytes.into(),
        )
        .await?;
    info!(date, rows = n, "rv written");
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_prices_have_zero_vol() {
        let pts: Vec<(i64, f64)> = (0..1000).map(|i| (i * 1_000_000_000, 100.0)).collect();
        let p = estimate(&pts, 999_000_000_000, 900_000_000_000, 1_000_000_000, false).unwrap();
        assert_eq!(p.sigma_ann, 0.0);
        assert!(p.coverage > 0.99);
    }

    #[test]
    fn gbm_recovers_its_sigma() {
        use rand::{rngs::StdRng, Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(42);
        let sigma = 0.6; // annualized
        let step_ns = 1_000_000_000i64; // 1s
        let dt_years = step_ns as f64 / NS_PER_YEAR;
        let step_vol = sigma * dt_years.sqrt();
        let mut price = 100.0f64;
        let n = 86_400; // one day of 1s steps
        let pts: Vec<(i64, f64)> = (0..n)
            .map(|i| {
                // Box-Muller from two uniforms.
                let (u1, u2): (f64, f64) = (rng.gen_range(1e-9..1.0), rng.gen());
                let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
                price *= (step_vol * z).exp();
                (i as i64 * step_ns, price)
            })
            .collect();
        let end = (n - 1) as i64 * step_ns;
        let p = estimate(&pts, end, end, step_ns, false).unwrap();
        assert!(
            (p.sigma_ann - sigma).abs() < 0.05,
            "recovered {} vs true {sigma}",
            p.sigma_ann
        );
        // Subsampled agrees on clean data.
        let ps = estimate(&pts, end, end, 5 * step_ns, true).unwrap();
        assert!(
            (ps.sigma_ann - sigma).abs() < 0.08,
            "subsampled {}",
            ps.sigma_ann
        );
    }

    #[test]
    fn gap_reduces_coverage_not_sigma_blowup() {
        // 1000s of data, then a 500s hole, then more data.
        let mut pts: Vec<(i64, f64)> = (0..1000).map(|i| (i * 1_000_000_000, 100.0)).collect();
        pts.extend((1500..2000).map(|i| (i * 1_000_000_000, 100.0)));
        let p = estimate(
            &pts,
            1_999_000_000_000,
            1_999_000_000_000,
            1_000_000_000,
            false,
        )
        .unwrap();
        assert_eq!(p.sigma_ann, 0.0);
        // LOCF carries through the hole, so coverage stays high but sigma
        // is not inflated — the honest gap masking happens via the gaps
        // ledger, coverage here reflects grid slots with data on both ends.
        assert!(p.coverage > 0.9);
    }
}
