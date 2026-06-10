//! CSV loaders (doc 06 §5).
//!
//! Candle schema (`data/<asset>_usd_1d.csv`), header required:
//!
//! ```csv
//! ts_ms,open,high,low,close
//! 1684108800000,1.045,1.092,1.001,1.067
//! ```
//!
//! Vol-index schema (`data/<asset>_dvol_1d.csv`) — `dvol` in index points
//! (Deribit convention: 55.0 ⇒ 55% annualized; the loader converts to a
//! fraction):
//!
//! ```csv
//! ts_ms,dvol
//! 1684108800000,55.0
//! ```
//!
//! Raw files stay out of git (`data/` is gitignored); fetch scripts live
//! in `tools/backtester/fetch-data.sh`.

use std::path::Path as FsPath;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use vault_sim::paths::Bar;

#[derive(Debug, Deserialize)]
struct CandleRow {
    ts_ms: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
}

pub fn load_candles(path: &FsPath) -> Result<Vec<Bar>> {
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("opening candle file {}", path.display()))?;
    let mut bars = Vec::new();
    for row in rdr.deserialize() {
        let r: CandleRow = row.context("parsing candle row")?;
        ensure!(r.close > 0.0 && r.close.is_finite(), "non-positive close at ts {}", r.ts_ms);
        bars.push(Bar { ts_ms: r.ts_ms, open: r.open, high: r.high, low: r.low, close: r.close });
    }
    ensure!(!bars.is_empty(), "empty candle file {}", path.display());
    ensure!(
        bars.windows(2).all(|w| w[0].ts_ms < w[1].ts_ms),
        "candles must be strictly increasing in ts_ms"
    );
    Ok(bars)
}

#[derive(Debug, Deserialize)]
struct VolRow {
    ts_ms: u64,
    dvol: f64,
}

/// Loads a vol-index series and aligns it to `bars` by timestamp: each bar
/// takes the most recent index value at or before its `ts_ms`. Bars before
/// the first index print get the first value (warm-up approximation).
pub fn load_vol_index_aligned(path: &FsPath, bars: &[Bar]) -> Result<Vec<f64>> {
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("opening vol index file {}", path.display()))?;
    let mut points: Vec<(u64, f64)> = Vec::new();
    for row in rdr.deserialize() {
        let r: VolRow = row.context("parsing vol row")?;
        ensure!(r.dvol > 0.0 && r.dvol.is_finite(), "non-positive dvol at ts {}", r.ts_ms);
        points.push((r.ts_ms, r.dvol / 100.0));
    }
    ensure!(!points.is_empty(), "empty vol index file {}", path.display());
    ensure!(
        points.windows(2).all(|w| w[0].0 < w[1].0),
        "vol index must be strictly increasing in ts_ms"
    );

    let mut out = Vec::with_capacity(bars.len());
    let mut i = 0usize;
    for bar in bars {
        while i + 1 < points.len() && points[i + 1].0 <= bar.ts_ms {
            i += 1;
        }
        out.push(points[i].1);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("backtester-test-{name}"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn loads_candles() {
        let p = write_tmp(
            "candles.csv",
            "ts_ms,open,high,low,close\n1000,1.0,1.2,0.9,1.1\n2000,1.1,1.3,1.0,1.2\n",
        );
        let bars = load_candles(&p).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[1].close, 1.2);
    }

    #[test]
    fn rejects_unsorted_candles() {
        let p = write_tmp(
            "unsorted.csv",
            "ts_ms,open,high,low,close\n2000,1,1,1,1\n1000,1,1,1,1\n",
        );
        assert!(load_candles(&p).is_err());
    }

    #[test]
    fn aligns_vol_index_step_function() {
        let candles = write_tmp(
            "c2.csv",
            "ts_ms,open,high,low,close\n500,1,1,1,1\n1500,1,1,1,1\n2500,1,1,1,1\n3500,1,1,1,1\n",
        );
        let vols = write_tmp("v2.csv", "ts_ms,dvol\n1000,50\n3000,80\n");
        let bars = load_candles(&candles).unwrap();
        let iv = load_vol_index_aligned(&vols, &bars).unwrap();
        // 500 → warm-up (first value), 1500 → 0.5, 2500 → 0.5, 3500 → 0.8.
        assert_eq!(iv, vec![0.5, 0.5, 0.5, 0.8]);
    }
}
