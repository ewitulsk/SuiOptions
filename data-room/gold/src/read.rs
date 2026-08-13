//! Silver readers: (ts, price) series out of trades / book_top
//! partitions. Timestamp is `ts_event` when present, else `ts_recv` —
//! vision rows only carry event time, live rows carry both.

use arrow::array::{Array, Float64Array, Int64Array};
use futures::TryStreamExt;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::Store;

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

async fn read_partition(
    store: &Store,
    key: &str,
) -> anyhow::Result<Vec<arrow::array::RecordBatch>> {
    let bytes = match store.get(&object_store::path::Path::from(key)).await {
        Ok(r) => r.bytes().await?,
        Err(object_store::Error::NotFound { .. }) => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
    Ok(reader.collect::<Result<_, _>>()?)
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

/// Trade (ts, price, size) rows for one (exchange, symbol, date).
pub async fn trades(
    store: &Store,
    exchange: &str,
    symbol: &str,
    date: &str,
) -> anyhow::Result<Vec<(i64, f64, f64)>> {
    let key = schema::silver_key("trades", exchange, symbol, date);
    let mut out = Vec::new();
    for b in read_partition(store, &key).await? {
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
            out.push((ts, price.value(i), size.value(i)));
        }
    }
    Ok(out)
}

/// Mid-quote (ts, mid) rows for one (exchange, symbol, date).
pub async fn mids(
    store: &Store,
    exchange: &str,
    symbol: &str,
    date: &str,
) -> anyhow::Result<Vec<(i64, f64)>> {
    let key = schema::silver_key("book_top", exchange, symbol, date);
    let mut out = Vec::new();
    for b in read_partition(store, &key).await? {
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
            out.push((ts, (bid.value(i) + ask.value(i)) / 2.0));
        }
    }
    Ok(out)
}

/// (ts, price) over a date range (inclusive), sorted by ts.
pub async fn price_series(
    store: &Store,
    exchange: &str,
    symbol: &str,
    source: &str,
    dates: &[String],
) -> anyhow::Result<Vec<(i64, f64)>> {
    let mut out = Vec::new();
    for d in dates {
        match source {
            "trades" => out.extend(
                trades(store, exchange, symbol, d)
                    .await?
                    .into_iter()
                    .map(|(t, p, _)| (t, p)),
            ),
            "mid" => out.extend(mids(store, exchange, symbol, d).await?),
            other => anyhow::bail!("unknown source {other}"),
        }
    }
    out.sort_by_key(|(t, _)| *t);
    Ok(out)
}
