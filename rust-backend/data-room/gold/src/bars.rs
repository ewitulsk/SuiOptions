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
    /// ts of the first/last trade seen for this bucket — open/close are
    /// tracked by timestamp so out-of-order silver rows aggregate right.
    first_ts: i64,
    last_ts: i64,
}

/// Order-independent OHLCV aggregation keyed by bucket start.
#[derive(Default)]
struct BarMap {
    freq_ns: i64,
    day_start_ns: i64,
    map: std::collections::BTreeMap<i64, Bar>,
}

impl BarMap {
    fn new(day_start_ns: i64, freq_ns: i64) -> Self {
        Self {
            freq_ns,
            day_start_ns,
            map: Default::default(),
        }
    }

    fn add(&mut self, ts: i64, px: f64, sz: f64) {
        let bucket = (ts - self.day_start_ns).div_euclid(self.freq_ns);
        let ts_open = self.day_start_ns + bucket * self.freq_ns;
        let b = self.map.entry(ts_open).or_insert(Bar {
            ts_open,
            open: px,
            high: px,
            low: px,
            close: px,
            volume: 0.0,
            n: 0,
            first_ts: ts,
            last_ts: ts,
        });
        if ts < b.first_ts {
            b.first_ts = ts;
            b.open = px;
        }
        if ts >= b.last_ts {
            b.last_ts = ts;
            b.close = px;
        }
        b.high = b.high.max(px);
        b.low = b.low.min(px);
        b.volume += sz;
        b.n += 1;
    }
}

/// `symbols` restricts the pairs (any exchange) when non-empty — backfills
/// of one underlying must not re-crunch every dense BTC day.
pub async fn compute_day(store: &Store, date: &str, symbols: &[String]) -> anyhow::Result<usize> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let day_start_ns = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap();

    let mut files = 0usize;
    for (exchange, symbol) in crate::pairs_for_date(store, date).await? {
        if !symbols.is_empty() && !symbols.iter().any(|s| s.eq_ignore_ascii_case(&symbol)) {
            continue;
        }
        // One streaming pass fills all frequencies; memory = bucket maps
        // (≤ ~90k entries), never the day's rows.
        let mut maps: Vec<BarMap> = FREQS_S
            .iter()
            .map(|f| BarMap::new(day_start_ns, f * 1_000_000_000))
            .collect();
        read::stream_trades(store, &exchange, &symbol, date, |ts, px, sz| {
            for m in maps.iter_mut() {
                m.add(ts, px, sz);
            }
        })
        .await?;
        if maps[0].map.is_empty() {
            continue;
        }
        let instrument = crate::instrument_id(&exchange, &symbol);
        for (i, m) in maps.into_iter().enumerate() {
            let freq_s = FREQS_S[i];
            let bars: Vec<Bar> = m.map.into_values().collect();
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
            let bytes = data_room_schema::write_parquet(&batch)?;
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
        let mut m = BarMap::new(0, ns);
        for (ts, px, sz) in [
            (10 * ns, 100.0, 1.0),
            (10 * ns + 1, 105.0, 2.0),
            (10 * ns + 2, 95.0, 1.0),
            (11 * ns, 96.0, 1.0), // next 1s bucket
        ] {
            m.add(ts, px, sz);
        }
        let bars: Vec<&Bar> = m.map.values().collect();
        assert_eq!(bars.len(), 2);
        let b = bars[0];
        assert_eq!((b.open, b.high, b.low, b.close), (100.0, 105.0, 95.0, 95.0));
        assert_eq!(b.volume, 4.0);
        assert_eq!(b.n, 3);
        assert_eq!(bars[1].open, 96.0);
    }

    #[test]
    fn bars_are_order_independent() {
        // Out-of-order silver (the 2023-01 dump quirk) must aggregate the
        // same as sorted input: open/close keyed by timestamp, not arrival.
        let ns = 1_000_000_000i64;
        let mut m = BarMap::new(0, 60 * ns);
        for (ts, px) in [
            (30 * ns, 101.0),
            (5 * ns, 99.0),
            (55 * ns, 103.0),
            (12 * ns, 100.0),
        ] {
            m.add(ts, px, 1.0);
        }
        let b = m.map.values().next().unwrap();
        assert_eq!(b.open, 99.0, "open = earliest ts, not first arrival");
        assert_eq!(b.close, 103.0, "close = latest ts");
        assert_eq!((b.high, b.low), (103.0, 99.0));
    }
}
