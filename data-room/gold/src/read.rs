//! Silver readers with BOUNDED memory: a dense day holds 20M+ trades
//! (May 2021, 2024+), and 8-day RV windows over such days OOM'd the 2 GB
//! host when loaded as row vectors. Rows now stream batch-by-batch into
//! per-row callbacks; series consumers get a 200 ms slot-reduced price
//! series (exact for every RV grid we compute — all grid offsets are
//! multiples of 200 ms) instead of raw rows.
//!
//! Timestamp is `ts_event` when present, else `ts_recv` — vision rows
//! only carry event time, live rows carry both (book_top prefers
//! `ts_recv`, its capture axis).

use arrow::array::{Array, Float64Array, Int64Array};
use futures::TryStreamExt;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::Store;

/// Slot width of the reduced price series. Every RV sampling grid
/// (intervals {1..300}s, subsample offsets j*interval/5) lands on
/// multiples of this, so last-observation reduction at this granularity
/// is exact for LOCF sampling at any grid point.
pub const REDUCE_SLOT_MS: i64 = 200;

pub async fn list_keys(store: &Store, prefix: &str) -> anyhow::Result<Vec<String>> {
    let path = object_store::path::Path::from(prefix.trim_end_matches('/'));
    let mut keys: Vec<String> = store
        .list(Some(&path))
        .map_ok(|m| m.location.to_string())
        .try_collect()
        .await?;
    keys.sort();
    Ok(keys)
}

fn col_i64(b: &arrow::array::RecordBatch, name: &str) -> anyhow::Result<Int64Array> {
    Ok(b.column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow::anyhow!("{name} not i64"))?
        .clone())
}

fn col_f64(b: &arrow::array::RecordBatch, name: &str) -> anyhow::Result<Float64Array> {
    Ok(b.column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow::anyhow!("{name} not f64"))?
        .clone())
}

/// Stream one partition's trades as (ts ns, price, size) — one batch in
/// memory at a time. Missing partition = no-op.
pub async fn stream_trades(
    store: &Store,
    exchange: &str,
    symbol: &str,
    date: &str,
    mut f: impl FnMut(i64, f64, f64),
) -> anyhow::Result<()> {
    let key = schema::silver_key("trades", exchange, symbol, date);
    let bytes = match store
        .get(&object_store::path::Path::from(key.as_str()))
        .await
    {
        Ok(r) => r.bytes().await?,
        Err(object_store::Error::NotFound { .. }) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
    for batch in reader {
        let b = batch?;
        let ts_event = col_i64(&b, "ts_event")?;
        let ts_recv = col_i64(&b, "ts_recv")?;
        let price = col_f64(&b, "price")?;
        let size = col_f64(&b, "size")?;
        for i in 0..b.num_rows() {
            let ts = if ts_event.is_valid(i) {
                ts_event.value(i)
            } else if ts_recv.is_valid(i) {
                ts_recv.value(i)
            } else {
                continue;
            };
            f(ts, price.value(i), size.value(i));
        }
    }
    Ok(())
}

/// Stream one partition's book_top as (ts ns, mid).
pub async fn stream_mids(
    store: &Store,
    exchange: &str,
    symbol: &str,
    date: &str,
    mut f: impl FnMut(i64, f64),
) -> anyhow::Result<()> {
    let key = schema::silver_key("book_top", exchange, symbol, date);
    let bytes = match store
        .get(&object_store::path::Path::from(key.as_str()))
        .await
    {
        Ok(r) => r.bytes().await?,
        Err(object_store::Error::NotFound { .. }) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
    for batch in reader {
        let b = batch?;
        let ts_event = col_i64(&b, "ts_event")?;
        let ts_recv = col_i64(&b, "ts_recv")?;
        let bid = col_f64(&b, "bid_px")?;
        let ask = col_f64(&b, "ask_px")?;
        for i in 0..b.num_rows() {
            let ts = if ts_recv.is_valid(i) {
                ts_recv.value(i)
            } else if ts_event.is_valid(i) {
                ts_event.value(i)
            } else {
                continue;
            };
            f(ts, (bid.value(i) + ask.value(i)) / 2.0);
        }
    }
    Ok(())
}

/// (ts ns, price) series over a date range, reduced to the last
/// observation per 200 ms slot, ascending and bounded in memory by the
/// span (≤ ~84 MB for an 8-day RV window) regardless of row count.
/// Rows outside [span_start_ns, span_end_ns) are dropped at reduce time.
pub async fn reduced_price_series(
    store: &Store,
    exchange: &str,
    symbol: &str,
    source: &str,
    dates: &[String],
    span_start_ns: i64,
    span_end_ns: i64,
) -> anyhow::Result<Vec<(i64, f64)>> {
    let slot_ns = REDUCE_SLOT_MS * 1_000_000;
    let slots = ((span_end_ns - span_start_ns) / slot_ns + 1) as usize;
    // (last ts seen in slot, its price); ts=i64::MIN = empty.
    let mut reduced: Vec<(i64, f64)> = vec![(i64::MIN, 0.0); slots];
    {
        let mut visit = |ts: i64, px: f64| {
            if ts < span_start_ns || ts >= span_end_ns {
                return;
            }
            let slot = ((ts - span_start_ns) / slot_ns) as usize;
            if ts >= reduced[slot].0 {
                reduced[slot] = (ts, px);
            }
        };
        for d in dates {
            match source {
                "trades" => {
                    stream_trades(store, exchange, symbol, d, |ts, px, _| visit(ts, px)).await?
                }
                "mid" => stream_mids(store, exchange, symbol, d, &mut visit).await?,
                other => anyhow::bail!("unknown source {other}"),
            }
        }
    }
    Ok(reduced
        .into_iter()
        .filter(|(ts, _)| *ts != i64::MIN)
        .collect())
}
