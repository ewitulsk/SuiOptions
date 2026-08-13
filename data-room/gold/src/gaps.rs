//! Capture-gap ledger (spec §5.7): disconnect→connect intervals per
//! stream, derived from the collector's bronze marker lines.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use arrow::array::{Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use chrono::NaiveDate;
use serde::Deserialize;
use tracing::info;

use crate::{read, Store};

#[derive(Deserialize)]
struct BronzeLine {
    ts_recv_ns: i64,
    marker: Option<String>,
}

fn gaps_schema() -> Schema {
    Schema::new(vec![
        Field::new("exchange", DataType::Utf8, false),
        Field::new("stream", DataType::Utf8, false),
        Field::new("gap_start", DataType::Int64, false),
        Field::new("gap_end", DataType::Int64, false),
        Field::new("kind", DataType::Utf8, false),
    ])
}

/// disconnect→connect pairs, plus a trailing gap to end-of-day if the
/// day ends disconnected.
fn intervals(mut markers: Vec<(i64, String)>, day_end_ns: i64) -> Vec<(i64, i64)> {
    markers.sort();
    let mut out = Vec::new();
    let mut open: Option<i64> = None;
    for (ts, ev) in markers {
        match (ev.as_str(), open) {
            ("disconnect", None) => open = Some(ts),
            ("connect", Some(start)) => {
                out.push((start, ts));
                open = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        out.push((start, day_end_ns));
    }
    out
}

/// Exchanges whose bronze markers are audited. Extend when a venue
/// gains a live collector connection.
const EXCHANGES: &[&str] = &["coinbase", "hyperliquid"];

pub async fn compute_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let day_end_ns = (day + chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_nanos_opt()
        .unwrap();

    // marker lines per (exchange, stream) from every bronze file of
    // the day, across all collected exchanges.
    let mut keys: Vec<String> = Vec::new();
    for ex in EXCHANGES {
        keys.extend(read::list_keys(store, &format!("bronze/v1/exchange={ex}/")).await?);
    }
    let mut per_stream: BTreeMap<(String, String), Vec<(i64, String)>> = BTreeMap::new();
    for key in keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
    {
        let Some(stream) = key
            .split("/stream=")
            .nth(1)
            .and_then(|s| s.split('/').next())
        else {
            continue;
        };
        let Some(exchange) = key
            .split("/exchange=")
            .nth(1)
            .and_then(|s| s.split('/').next())
        else {
            continue;
        };
        let gz = store
            .get(&object_store::path::Path::from(key.as_str()))
            .await?
            .bytes()
            .await?;
        let mut raw = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut raw)?;
        for line in raw.lines() {
            if let Ok(bl) = serde_json::from_str::<BronzeLine>(line) {
                if let Some(m) = bl.marker {
                    per_stream
                        .entry((exchange.to_string(), stream.to_string()))
                        .or_default()
                        .push((bl.ts_recv_ns, m));
                }
            }
        }
    }

    let mut rows: Vec<(String, String, i64, i64)> = Vec::new();
    for ((exchange, stream), markers) in per_stream {
        for (start, end) in intervals(markers, day_end_ns) {
            rows.push((exchange.clone(), stream.clone(), start, end));
        }
    }

    let n = rows.len();
    let batch = RecordBatch::try_new(
        Arc::new(gaps_schema()),
        vec![
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|(e, _, _, _)| e.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|(_, s, _, _)| s.as_str()),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|(_, _, s, _)| *s),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|(_, _, _, e)| *e),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|_| "disconnect"),
            )),
        ],
    )?;
    let bytes = schema::write_parquet(&batch)?;
    store
        .put(
            &object_store::path::Path::from(format!("gold/v1/gaps/date={date}/part-00.parquet")),
            bytes.into(),
        )
        .await?;
    info!(date, gaps = n, "gaps written");
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_disconnect_connect_and_trails_open_gap() {
        let m = vec![
            (100, "disconnect".to_string()),
            (200, "connect".to_string()),
            (500, "disconnect".to_string()),
        ];
        assert_eq!(intervals(m, 1000), vec![(100, 200), (500, 1000)]);
    }

    #[test]
    fn connect_without_open_gap_is_ignored() {
        let m = vec![
            (50, "connect".to_string()),
            (100, "disconnect".to_string()),
            (150, "connect".to_string()),
        ];
        assert_eq!(intervals(m, 1000), vec![(100, 150)]);
    }
}
