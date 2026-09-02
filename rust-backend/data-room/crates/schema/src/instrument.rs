//! Instrument master (spec §5.5): our `instrument_id` mapped to each
//! venue's native symbol plus static attributes. Full-snapshot table,
//! rewritten per snapshot date.

use std::sync::Arc;

use arrow::array::{Float64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Instrument {
    pub instrument_id: String,
    pub exchange: String,
    pub native_symbol: String,
    /// "spot" | "perp" | "option" | "future"
    pub asset_class: String,
    pub base: String,
    pub quote: String,
    pub tick_size: Option<f64>,
    pub lot_size: Option<f64>,
    pub strike: Option<f64>,
    /// ns epoch of expiry (options/futures).
    pub expiry: Option<i64>,
    /// "call" | "put".
    pub opt_type: Option<String>,
}

impl Instrument {
    /// Our id convention: `<base>-<quote>.<exchange>`, lowercase.
    pub fn make_id(base: &str, quote: &str, exchange: &str) -> String {
        format!(
            "{}-{}.{}",
            base.to_lowercase(),
            quote.to_lowercase(),
            exchange
        )
    }

    /// The `symbol=` partition value used in silver keys: `BASE-QUOTE`.
    pub fn symbol(&self) -> String {
        format!("{}-{}", self.base.to_uppercase(), self.quote.to_uppercase())
    }
}

pub fn instruments_schema() -> Schema {
    Schema::new(vec![
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("exchange", DataType::Utf8, false),
        Field::new("native_symbol", DataType::Utf8, false),
        Field::new("asset_class", DataType::Utf8, false),
        Field::new("base", DataType::Utf8, false),
        Field::new("quote", DataType::Utf8, false),
        Field::new("tick_size", DataType::Float64, true),
        Field::new("lot_size", DataType::Float64, true),
        Field::new("strike", DataType::Float64, true),
        Field::new("expiry", DataType::Int64, true),
        Field::new("opt_type", DataType::Utf8, true),
    ])
}

pub fn instruments_batch(mut rows: Vec<Instrument>) -> anyhow::Result<RecordBatch> {
    rows.sort_by(|a, b| a.instrument_id.cmp(&b.instrument_id));
    let batch = RecordBatch::try_new(
        Arc::new(instruments_schema()),
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.instrument_id.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.exchange.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.native_symbol.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.asset_class.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.base.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.quote.as_str()),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.tick_size).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.lot_size).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                rows.iter().map(|r| r.strike).collect::<Vec<_>>(),
            )),
            Arc::new(arrow::array::Int64Array::from(
                rows.iter().map(|r| r.expiry).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from_iter(
                rows.iter().map(|r| r.opt_type.as_deref()),
            )),
        ],
    )?;
    Ok(batch)
}

pub fn instruments_key(snapshot_date: &str) -> String {
    format!("silver/v1/instruments/snapshot_date={snapshot_date}/instruments.parquet")
}
