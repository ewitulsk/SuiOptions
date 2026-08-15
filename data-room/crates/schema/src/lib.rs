//! Canonical event types, Arrow schemas, and layout conventions for the
//! data room (docs/data-room-spec.md §5).
//!
//! Conventions enforced here:
//! - timestamps are i64 nanoseconds UTC; `ts_recv` is null only for
//!   bulk-archive-sourced rows (§6.6), never for live capture.
//! - prices/sizes are f64 in normalized human units.
//! - rows are sorted by (`ts_recv` else `ts_event`, tiebreak `src_line`)
//!   before writing, and the parquet writer settings are fixed, so the
//!   same input always produces byte-identical silver (§7 determinism).

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties, WriterVersion};

pub mod instrument;

pub use instrument::Instrument;

/// One canonical market-data event, produced by an exchange adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalEvent {
    Trade(Trade),
    BookTop(BookTop),
    Funding(FundingRate),
    OptionQuote(OptionsQuote),
    /// Collector lifecycle marker (connect/disconnect) — consumed by the
    /// gold `gaps` job, never normalized into silver tables.
    Marker(Marker),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub ts_event: Option<i64>,
    /// None for bulk-archive rows (vision dumps).
    pub ts_recv: Option<i64>,
    pub exchange: String,
    pub instrument_id: String,
    pub price: f64,
    pub size: f64,
    /// Aggressor side: "buy" / "sell" when known.
    pub side: Option<String>,
    pub trade_id: String,
    pub src_file: String,
    pub src_line: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BookTop {
    pub ts_event: Option<i64>,
    pub ts_recv: Option<i64>,
    pub exchange: String,
    pub instrument_id: String,
    pub update_id: i64,
    pub bid_px: f64,
    pub bid_sz: f64,
    pub ask_px: f64,
    pub ask_sz: f64,
    pub src_file: String,
    pub src_line: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FundingRate {
    pub ts_event: Option<i64>,
    pub ts_recv: Option<i64>,
    pub exchange: String,
    pub instrument_id: String,
    /// Per-interval rate as the venue quotes it.
    pub rate: f64,
    pub interval_hours: f64,
    /// "predicted" (streamed live estimate) | "settled" (finalized).
    pub kind: String,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
    pub src_file: String,
    pub src_line: i32,
}

/// One option contract's state within a chain snapshot (spec §5.4).
/// Prices are venue-native units (Deribit quotes options in the base
/// coin, e.g. BTC per contract); `mark_iv` is percent-annualized as the
/// venue reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsQuote {
    pub ts_event: Option<i64>,
    pub ts_recv: Option<i64>,
    pub exchange: String,
    pub instrument_id: String,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub mark_price: Option<f64>,
    pub mark_iv: Option<f64>,
    pub underlying_price: Option<f64>,
    pub open_interest: Option<f64>,
    pub src_file: String,
    pub src_line: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub ts_recv: i64,
    pub exchange: String,
    pub stream: String,
    /// "connect" | "disconnect"
    pub event: String,
}

// -- Arrow schemas ------------------------------------------------------

fn lineage_fields() -> Vec<Field> {
    vec![
        Field::new("src_file", DataType::Utf8, false),
        Field::new("src_line", DataType::Int32, false),
    ]
}

pub fn trades_schema() -> Schema {
    let mut f = vec![
        Field::new("ts_event", DataType::Int64, true),
        Field::new("ts_recv", DataType::Int64, true),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("price", DataType::Float64, false),
        Field::new("size", DataType::Float64, false),
        Field::new("side", DataType::Utf8, true),
        Field::new("trade_id", DataType::Utf8, false),
    ];
    f.extend(lineage_fields());
    Schema::new(f)
}

pub fn book_top_schema() -> Schema {
    let mut f = vec![
        Field::new("ts_event", DataType::Int64, true),
        Field::new("ts_recv", DataType::Int64, true),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("update_id", DataType::Int64, false),
        Field::new("bid_px", DataType::Float64, false),
        Field::new("bid_sz", DataType::Float64, false),
        Field::new("ask_px", DataType::Float64, false),
        Field::new("ask_sz", DataType::Float64, false),
    ];
    f.extend(lineage_fields());
    Schema::new(f)
}

// -- RecordBatch builders ----------------------------------------------

fn opt_i64(vals: impl Iterator<Item = Option<i64>>) -> ArrayRef {
    Arc::new(Int64Array::from(vals.collect::<Vec<_>>()))
}

/// Sort key shared by the batch builders: capture time when we have it,
/// else event time, tiebroken by lineage so ordering is total and stable.
fn sort_key(
    ts_recv: Option<i64>,
    ts_event: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> (i64, &str, i32) {
    (ts_recv.or(ts_event).unwrap_or(0), src_file, src_line)
}

pub fn trades_batch(mut rows: Vec<Trade>) -> anyhow::Result<RecordBatch> {
    rows.sort_by(|a, b| {
        sort_key(a.ts_recv, a.ts_event, &a.src_file, a.src_line).cmp(&sort_key(
            b.ts_recv,
            b.ts_event,
            &b.src_file,
            b.src_line,
        ))
    });
    let batch = RecordBatch::try_new(
        Arc::new(trades_schema()),
        vec![
            opt_i64(rows.iter().map(|r| r.ts_event)),
            opt_i64(rows.iter().map(|r| r.ts_recv)),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.price))),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.size))),
            Arc::new(StringArray::from_iter(
                rows.iter().map(|r| r.side.as_deref()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.trade_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.src_file.as_str()),
            )),
            Arc::new(Int32Array::from_iter_values(
                rows.iter().map(|r| r.src_line),
            )),
        ],
    )?;
    Ok(batch)
}

pub fn book_top_batch(mut rows: Vec<BookTop>) -> anyhow::Result<RecordBatch> {
    rows.sort_by(|a, b| {
        sort_key(a.ts_recv, a.ts_event, &a.src_file, a.src_line).cmp(&sort_key(
            b.ts_recv,
            b.ts_event,
            &b.src_file,
            b.src_line,
        ))
    });
    let batch = RecordBatch::try_new(
        Arc::new(book_top_schema()),
        vec![
            opt_i64(rows.iter().map(|r| r.ts_event)),
            opt_i64(rows.iter().map(|r| r.ts_recv)),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.update_id),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.bid_px),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.bid_sz),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.ask_px),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.ask_sz),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.src_file.as_str()),
            )),
            Arc::new(Int32Array::from_iter_values(
                rows.iter().map(|r| r.src_line),
            )),
        ],
    )?;
    Ok(batch)
}

pub fn funding_rates_schema() -> Schema {
    let mut f = vec![
        Field::new("ts_event", DataType::Int64, true),
        Field::new("ts_recv", DataType::Int64, true),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("rate", DataType::Float64, false),
        Field::new("interval_hours", DataType::Float64, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("mark_price", DataType::Float64, true),
        Field::new("index_price", DataType::Float64, true),
    ];
    f.extend(lineage_fields());
    Schema::new(f)
}

pub fn funding_rates_batch(mut rows: Vec<FundingRate>) -> anyhow::Result<RecordBatch> {
    rows.sort_by(|a, b| {
        sort_key(a.ts_recv, a.ts_event, &a.src_file, a.src_line).cmp(&sort_key(
            b.ts_recv,
            b.ts_event,
            &b.src_file,
            b.src_line,
        ))
    });
    let batch = RecordBatch::try_new(
        Arc::new(funding_rates_schema()),
        vec![
            opt_i64(rows.iter().map(|r| r.ts_event)),
            opt_i64(rows.iter().map(|r| r.ts_recv)),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.rate))),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.interval_hours),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.kind.as_str()),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.mark_price).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.index_price).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.src_file.as_str()),
            )),
            Arc::new(Int32Array::from_iter_values(
                rows.iter().map(|r| r.src_line),
            )),
        ],
    )?;
    Ok(batch)
}

pub fn options_quotes_schema() -> Schema {
    let mut f = vec![
        Field::new("ts_event", DataType::Int64, true),
        Field::new("ts_recv", DataType::Int64, true),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("bid", DataType::Float64, true),
        Field::new("ask", DataType::Float64, true),
        Field::new("mark_price", DataType::Float64, true),
        Field::new("mark_iv", DataType::Float64, true),
        Field::new("underlying_price", DataType::Float64, true),
        Field::new("open_interest", DataType::Float64, true),
    ];
    f.extend(lineage_fields());
    Schema::new(f)
}

pub fn options_quotes_batch(mut rows: Vec<OptionsQuote>) -> anyhow::Result<RecordBatch> {
    rows.sort_by(|a, b| {
        sort_key(a.ts_recv, a.ts_event, &a.src_file, a.src_line).cmp(&sort_key(
            b.ts_recv,
            b.ts_event,
            &b.src_file,
            b.src_line,
        ))
    });
    let batch = RecordBatch::try_new(
        Arc::new(options_quotes_schema()),
        vec![
            opt_i64(rows.iter().map(|r| r.ts_event)),
            opt_i64(rows.iter().map(|r| r.ts_recv)),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.bid).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.ask).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.mark_price).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.mark_iv).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.underlying_price).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.open_interest).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.src_file.as_str()),
            )),
            Arc::new(Int32Array::from_iter_values(
                rows.iter().map(|r| r.src_line),
            )),
        ],
    )?;
    Ok(batch)
}

/// options_quotes partitions by UNDERLYING, never per contract (spec §4:
/// a chain is hundreds of instruments — per-contract files would be the
/// classic small-files problem).
pub fn options_silver_key(exchange: &str, underlying: &str, date: &str) -> String {
    format!(
        "silver/v1/options_quotes/exchange={exchange}/underlying={underlying}/date={date}/part-00.parquet"
    )
}

/// Volatility-index candles (e.g. Deribit DVOL): index value is
/// percent-annualized implied vol.
pub fn vol_index_schema() -> Schema {
    Schema::new(vec![
        Field::new("ts", DataType::Int64, false),
        Field::new("open", DataType::Float64, false),
        Field::new("high", DataType::Float64, false),
        Field::new("low", DataType::Float64, false),
        Field::new("close", DataType::Float64, false),
    ])
}

pub fn vol_index_key(exchange: &str, symbol: &str, date: &str) -> String {
    format!("silver/v1/vol_index/exchange={exchange}/symbol={symbol}/date={date}/part-00.parquet")
}

/// funding_rates partitions split by row kind so the live (predicted)
/// normalizer and the settled backfill job overwrite independently:
/// `part-predicted.parquet` / `part-settled.parquet` in one partition dir.
pub fn funding_silver_key(exchange: &str, symbol: &str, date: &str, kind: &str) -> String {
    format!(
        "silver/v1/funding_rates/exchange={exchange}/symbol={symbol}/date={date}/part-{kind}.parquet"
    )
}

// -- Deterministic parquet writing --------------------------------------

/// Fixed writer settings: same rows in → byte-identical file out. Do not
/// add anything time- or environment-dependent (no created_by drift, no
/// multi-threaded row-group splits).
pub fn writer_props() -> WriterProperties {
    WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .set_max_row_group_size(1_048_576)
        .set_created_by("data-room".to_string())
        .build()
}

/// Serialize one batch to an in-memory parquet file with the fixed props.
pub fn write_parquet(batch: &RecordBatch) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), Some(writer_props()))?;
    w.write(batch)?;
    w.close()?;
    Ok(buf)
}

/// Incremental trades-parquet writer: accepts row chunks (already in
/// final order — callers feed time-ordered streams) and produces one
/// deterministic file. Exists so a dense day (2M+ perp trades) never
/// needs all its rows in memory at once; peak usage is one chunk.
pub struct TradesWriter {
    w: ArrowWriter<std::fs::File>,
    tmp: tempfile::NamedTempFile,
    rows: usize,
}

impl TradesWriter {
    /// Output accumulates in a TEMP FILE, not memory: a dense month holds
    /// 30+ concurrent day writers, and their combined compressed output
    /// (1.5 GB+ for 2023-01 BTCUSDT) OOM'd the 2 GB host when buffered.
    pub fn new() -> anyhow::Result<Self> {
        let tmp = tempfile::NamedTempFile::new()?;
        let file = tmp.reopen()?;
        let w = ArrowWriter::try_new(file, Arc::new(trades_schema()), Some(writer_props()))?;
        Ok(Self { w, tmp, rows: 0 })
    }

    /// Chunk rows MUST already be in cross-chunk sorted order; each chunk
    /// is order-normalized internally by `trades_batch`.
    pub fn write_chunk(&mut self, rows: Vec<Trade>) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.rows += rows.len();
        self.w.write(&trades_batch(rows)?)?;
        // Force the row group to disk NOW: with 30+ concurrent day
        // writers (out-of-order months), rows buffered toward the 1M-row
        // group threshold sum to >1.5 GB of arrow memory across writers.
        // One row group per chunk trades a little file-size overhead for
        // a hard memory bound.
        self.w.flush()?;
        Ok(())
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Close the parquet stream; the finished file lives at the returned
    /// temp path until dropped.
    pub fn finish(self) -> anyhow::Result<tempfile::NamedTempFile> {
        self.w.close()?;
        Ok(self.tmp)
    }
}

/// Same chunked, temp-file-backed writer as [`TradesWriter`], for
/// `book_top`. Needed for `bookTicker` dumps, which are far denser than
/// trades — a single SUIUSDT day is ~1.9M quotes and a busy month is
/// >100M — so buffering a day's rows is not an option.
pub struct BookTopWriter {
    w: ArrowWriter<std::fs::File>,
    tmp: tempfile::NamedTempFile,
    rows: usize,
}

impl BookTopWriter {
    pub fn new() -> anyhow::Result<Self> {
        let tmp = tempfile::NamedTempFile::new()?;
        let file = tmp.reopen()?;
        let w = ArrowWriter::try_new(file, Arc::new(book_top_schema()), Some(writer_props()))?;
        Ok(Self { w, tmp, rows: 0 })
    }

    /// Chunk rows MUST already be in cross-chunk sorted order; each chunk
    /// is order-normalized internally by `book_top_batch`.
    pub fn write_chunk(&mut self, rows: Vec<BookTop>) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.rows += rows.len();
        self.w.write(&book_top_batch(rows)?)?;
        // Same hard memory bound as TradesWriter: flush the row group now
        // rather than letting arrow buffer toward its 1M-row threshold
        // across 30+ concurrent day writers.
        self.w.flush()?;
        Ok(())
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn finish(self) -> anyhow::Result<tempfile::NamedTempFile> {
        self.w.close()?;
        Ok(self.tmp)
    }
}

// -- Layout helpers ------------------------------------------------------

/// silver partition key for a table, e.g.
/// `silver/v1/trades/exchange=coinbase/symbol=BTC-USD/date=2026-08-13/part-00.parquet`
pub fn silver_key(table: &str, exchange: &str, symbol: &str, date: &str) -> String {
    format!("silver/v1/{table}/exchange={exchange}/symbol={symbol}/date={date}/part-00.parquet")
}

pub fn bronze_ws_prefix(exchange: &str, stream: &str, date: &str) -> String {
    format!("bronze/v1/exchange={exchange}/stream={stream}/date={date}/")
}

pub fn bronze_vision_prefix(market: &str, kind: &str, symbol: &str) -> String {
    format!("bronze/v1/exchange=binance/source=vision/market={market}/kind={kind}/symbol={symbol}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(recv: Option<i64>, id: &str) -> Trade {
        Trade {
            ts_event: Some(1),
            ts_recv: recv,
            exchange: "coinbase".into(),
            instrument_id: "btc-usd.coinbase".into(),
            price: 100.0,
            size: 1.0,
            side: Some("buy".into()),
            trade_id: id.into(),
            src_file: "f".into(),
            src_line: 0,
        }
    }

    #[test]
    fn trades_batch_sorts_by_recv() {
        let b = trades_batch(vec![t(Some(20), "b"), t(Some(10), "a")]).unwrap();
        let ids: Vec<_> = b
            .column_by_name("trade_id")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .iter()
            .map(|s| s.unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn parquet_write_is_deterministic() {
        let rows = vec![t(Some(10), "a"), t(Some(20), "b")];
        let b1 = write_parquet(&trades_batch(rows.clone()).unwrap()).unwrap();
        let b2 = write_parquet(&trades_batch(rows).unwrap()).unwrap();
        assert_eq!(b1, b2, "same rows must produce byte-identical parquet");
    }
}
