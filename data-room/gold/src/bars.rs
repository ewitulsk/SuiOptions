//! OHLCV bars from silver trades (spec §5.7), one file per
//! (freq, exchange, symbol, date).

use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::NaiveDate;
use tracing::info;

use crate::{read, Store};

pub const FREQS_S: &[i64] = &[1, 60, 3_600];

fn bars_schema() -> Schema {
    Schema::new(vec![
        Field::new("ts_open", DataType::Int64, false),
        Field::new("freq_s", DataType::Int64, false),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("open", DataType::Float64, false),
        Field::new("high", DataType::Float64, false),
        Field::new("low", DataType::Float64, false),
        Field::new("close", DataType::Float64, false),
        Field::new("volume", DataType::Float64, false),
        Field::new("n_trades", DataType::Int64, false),
    ])
}

struct Bar {
    ts_open: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
    n: i64,
}

fn build_bars(trades: &[(i64, f64, f64)], day_start_ns: i64, freq_ns: i64) -> Vec<Bar> {
    let mut out: Vec<Bar> = Vec::new();
    for &(ts, px, sz) in trades {
        let bucket = (ts - day_start_ns).div_euclid(freq_ns);
        let ts_open = day_start_ns + bucket * freq_ns;
        match out.last_mut() {
            Some(b) if b.ts_open == ts_open => {
                b.high = b.high.max(px);
                b.low = b.low.min(px);
                b.close = px;
                b.volume += sz;
                b.n += 1;
            }
            _ => out.push(Bar {
                ts_open,
                open: px,
                high: px,
                low: px,
                close: px,
                volume: sz,
                n: 1,
            }),
        }
    }
    out
}

pub async fn compute_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let day_start_ns = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap();

    let mut files = 0usize;
    for (exchange, symbol) in crate::pairs_for_date(store, date).await? {
        let mut trades = read::trades(store, &exchange, &symbol, date).await?;
        trades.sort_by_key(|(t, _, _)| *t);
        if trades.is_empty() {
            continue;
        }
        let instrument = crate::instrument_id(&exchange, &symbol);
        for &freq_s in FREQS_S {
            let bars = build_bars(&trades, day_start_ns, freq_s * 1_000_000_000);
            if bars.is_empty() {
                continue;
            }
            let batch = RecordBatch::try_new(
                Arc::new(bars_schema()),
                vec![
                    Arc::new(Int64Array::from_iter_values(bars.iter().map(|b| b.ts_open))),
                    Arc::new(Int64Array::from_iter_values(bars.iter().map(|_| freq_s))),
                    Arc::new(StringArray::from_iter_values(
                        bars.iter().map(|_| exchange.as_str()),
                    )),
                    Arc::new(StringArray::from_iter_values(
                        bars.iter().map(|_| instrument.as_str()),
                    )),
                    Arc::new(Float64Array::from_iter_values(bars.iter().map(|b| b.open))),
                    Arc::new(Float64Array::from_iter_values(bars.iter().map(|b| b.high))),
                    Arc::new(Float64Array::from_iter_values(bars.iter().map(|b| b.low))),
                    Arc::new(Float64Array::from_iter_values(bars.iter().map(|b| b.close))),
                    Arc::new(Float64Array::from_iter_values(
                        bars.iter().map(|b| b.volume),
                    )),
                    Arc::new(Int64Array::from_iter_values(bars.iter().map(|b| b.n))),
                ],
            )?;
            let bytes = schema::write_parquet(&batch)?;
            let key = format!(
                "gold/v1/bars/freq={freq_s}s/exchange={exchange}/symbol={symbol}/date={date}/part-00.parquet"
            );
            store
                .put(&object_store::path::Path::from(key), bytes.into())
                .await?;
            files += 1;
        }
    }
    info!(date, files, "bars written");
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_aggregate_ohlcv() {
        let ns = 1_000_000_000i64;
        let trades = vec![
            (10 * ns, 100.0, 1.0),
            (10 * ns + 1, 105.0, 2.0),
            (10 * ns + 2, 95.0, 1.0),
            (11 * ns, 96.0, 1.0), // next 1s bucket
        ];
        let bars = build_bars(&trades, 0, ns);
        assert_eq!(bars.len(), 2);
        let b = &bars[0];
        assert_eq!((b.open, b.high, b.low, b.close), (100.0, 105.0, 95.0, 95.0));
        assert_eq!(b.volume, 4.0);
        assert_eq!(b.n, 3);
        assert_eq!(bars[1].open, 96.0);
    }
}
