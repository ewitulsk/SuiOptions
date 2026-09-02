//! Lake readers: gold bars, silver funding rows, silver vol index. One
//! parquet object at a time (the same bounded-memory rule as
//! data-room's `gold/read.rs`, which these mirror until the workspaces
//! merge — doc 09 §"Outstanding from doc 08", H versus G4).

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{Array, Float64Array, Int64Array};
use chrono::NaiveDate;
use object_store::{aws::AmazonS3Builder, local::LocalFileSystem, ObjectStore};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use url::Url;

pub type Store = Arc<dyn ObjectStore>;

/// `s3://bucket` (creds/endpoint from the environment) or `file:///path`.
pub fn open_store(store_url: &str) -> Result<Store> {
    let url = Url::parse(store_url).with_context(|| format!("bad store url {store_url}"))?;
    match url.scheme() {
        "s3" => {
            let bucket = url.host_str().context("s3 url missing bucket")?;
            Ok(Arc::new(AmazonS3Builder::from_env().with_bucket_name(bucket).build()?))
        }
        "file" => Ok(Arc::new(LocalFileSystem::new_with_prefix(url.path())?)),
        other => anyhow::bail!("unsupported store scheme {other}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    pub ts_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FundingRow {
    pub ts_ms: i64,
    /// Per-interval rate (Binance: per 8h), NOT annualized.
    pub rate: f64,
    pub interval_hours: f64,
}

pub fn dates(from: &str, to: &str) -> Result<Vec<String>> {
    let a = NaiveDate::parse_from_str(from, "%Y-%m-%d")?;
    let b = NaiveDate::parse_from_str(to, "%Y-%m-%d")?;
    anyhow::ensure!(a <= b, "from after to");
    Ok(a.iter_days().take_while(|d| *d <= b).map(|d| d.format("%Y-%m-%d").to_string()).collect())
}

pub fn date_start_ms(date: &str) -> Result<i64> {
    Ok(NaiveDate::parse_from_str(date, "%Y-%m-%d")?.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}

async fn get_bytes(store: &Store, key: &str) -> Result<Option<bytes::Bytes>> {
    match store.get(&object_store::path::Path::from(key)).await {
        Ok(r) => Ok(Some(r.bytes().await?)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn col_i64(b: &arrow::array::RecordBatch, name: &str) -> Result<Int64Array> {
    Ok(b.column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow::anyhow!("{name} not i64"))?
        .clone())
}

fn col_f64(b: &arrow::array::RecordBatch, name: &str) -> Result<Float64Array> {
    Ok(b.column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow::anyhow!("{name} not f64"))?
        .clone())
}

/// Lake timestamps are ns; tolerate ms in case a table changes units.
fn to_ms(ts: i64) -> i64 {
    if ts > 100_000_000_000_000 {
        ts / 1_000_000
    } else {
        ts
    }
}

/// 60-second bars for every date in the range, ascending; missing days
/// are simply absent (coverage is reported by the engine).
pub async fn load_bars(store: &Store, exchange: &str, symbol: &str, from: &str, to: &str) -> Result<Vec<Bar>> {
    let mut out = Vec::new();
    for d in dates(from, to)? {
        let key = format!("gold/v1/bars/freq=60s/exchange={exchange}/symbol={symbol}/date={d}/part-00.parquet");
        let Some(bytes) = get_bytes(store, &key).await? else { continue };
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
        for batch in reader {
            let b = batch?;
            let ts = col_i64(&b, "ts_open")?;
            let (o, h, l, c, v) = (col_f64(&b, "open")?, col_f64(&b, "high")?, col_f64(&b, "low")?, col_f64(&b, "close")?, col_f64(&b, "volume")?);
            for i in 0..b.num_rows() {
                out.push(Bar { ts_ms: to_ms(ts.value(i)), open: o.value(i), high: h.value(i), low: l.value(i), close: c.value(i), volume: v.value(i) });
            }
        }
    }
    out.sort_by_key(|b| b.ts_ms);
    out.dedup_by_key(|b| b.ts_ms);
    Ok(out)
}

/// Settled funding rows (the `part-settled` kind; the live predicted rows
/// are not history), ascending.
pub async fn load_funding(store: &Store, exchange: &str, symbol: &str, from: &str, to: &str) -> Result<Vec<FundingRow>> {
    let mut out = Vec::new();
    for d in dates(from, to)? {
        let key = format!("silver/v1/funding_rates/exchange={exchange}/symbol={symbol}/date={d}/part-settled.parquet");
        let Some(bytes) = get_bytes(store, &key).await? else { continue };
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
        for batch in reader {
            let b = batch?;
            let ts_event = col_i64(&b, "ts_event")?;
            let ts_recv = col_i64(&b, "ts_recv")?;
            let rate = col_f64(&b, "rate")?;
            let hours = col_f64(&b, "interval_hours")?;
            for i in 0..b.num_rows() {
                let ts = if ts_event.is_valid(i) { ts_event.value(i) } else if ts_recv.is_valid(i) { ts_recv.value(i) } else { continue };
                out.push(FundingRow { ts_ms: to_ms(ts), rate: rate.value(i), interval_hours: hours.value(i) });
            }
        }
    }
    out.sort_by_key(|r| r.ts_ms);
    out.dedup_by_key(|r| r.ts_ms);
    Ok(out)
}

/// Vol index closes (e.g. Deribit BTC-DVOL) as (ts_ms, close), ascending.
pub async fn load_vol_index(store: &Store, exchange: &str, symbol: &str, from: &str, to: &str) -> Result<Vec<(i64, f64)>> {
    let mut out = Vec::new();
    for d in dates(from, to)? {
        let key = format!("silver/v1/vol_index/exchange={exchange}/symbol={symbol}/date={d}/part-00.parquet");
        let Some(bytes) = get_bytes(store, &key).await? else { continue };
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
        for batch in reader {
            let b = batch?;
            let ts = col_i64(&b, "ts")?;
            let close = col_f64(&b, "close")?;
            for i in 0..b.num_rows() {
                out.push((to_ms(ts.value(i)), close.value(i)));
            }
        }
    }
    out.sort_by_key(|r| r.0);
    Ok(out)
}
