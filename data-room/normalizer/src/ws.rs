//! Websocket bronze (collector JSONL) → silver, one (stream, UTC day)
//! partition at a time. Exchange-generic: the adapter parse and the
//! stream→table mapping dispatch on the exchange name (spec §7.1).
//!
//! Stream → silver table:
//!   coinbase:    matches.P → trades, ticker.P → book_top
//!   hyperliquid: trades.C → trades, bbo.C → book_top,
//!                ctx.C → funding_rates (kind=predicted)

use std::collections::HashSet;
use std::io::Read;

use schema::CanonicalEvent;
use serde::Deserialize;
use tracing::info;

use crate::{get_bytes, handle_rejects, list_keys, put_bytes, Store};

#[derive(Deserialize)]
struct BronzeLine {
    ts_recv_ns: i64,
    #[allow(dead_code)]
    seq: u64,
    payload: Option<String>,
    marker: Option<String>,
}

fn parse_payload(
    exchange: &str,
    payload: &str,
    ts_recv: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<CanonicalEvent>, adapters::Reject> {
    match exchange {
        "hyperliquid" => adapters::hyperliquid::parse(payload, ts_recv, src_file, src_line),
        _ => adapters::coinbase::parse(payload, ts_recv, src_file, src_line),
    }
}

/// (silver table, symbol partition value) for a bronze stream name.
fn stream_target(exchange: &str, stream: &str) -> Option<(&'static str, String)> {
    let (kind, suffix) = stream.split_once('.')?;
    match (exchange, kind) {
        ("coinbase", "matches") => Some(("trades", suffix.to_string())),
        ("coinbase", "ticker") => Some(("book_top", suffix.to_string())),
        ("hyperliquid", "trades") => {
            Some(("trades", adapters::hyperliquid::partition_symbol(suffix)))
        }
        ("hyperliquid", "bbo") => {
            Some(("book_top", adapters::hyperliquid::partition_symbol(suffix)))
        }
        ("hyperliquid", "ctx") => Some((
            "funding_rates",
            adapters::hyperliquid::partition_symbol(suffix),
        )),
        _ => None,
    }
}

pub async fn streams_for_date(
    store: &Store,
    exchange: &str,
    date: &str,
) -> anyhow::Result<Vec<String>> {
    let keys = list_keys(store, &format!("bronze/v1/exchange={exchange}/")).await?;
    let mut streams: Vec<String> = keys
        .iter()
        .filter(|k| k.contains(&format!("/date={date}/")))
        .filter_map(|k| {
            k.split("/stream=")
                .nth(1)?
                .split('/')
                .next()
                .map(str::to_string)
        })
        .filter(|s| stream_target(exchange, s).is_some())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    streams.sort();
    Ok(streams)
}

/// Normalize every stream captured on `date`. Returns partition count.
pub async fn normalize_day(store: &Store, exchange: &str, date: &str) -> anyhow::Result<usize> {
    let streams = streams_for_date(store, exchange, date).await?;
    for stream in &streams {
        normalize_stream_day(store, exchange, stream, date).await?;
    }
    Ok(streams.len())
}

pub async fn normalize_stream_day(
    store: &Store,
    exchange: &str,
    stream: &str,
    date: &str,
) -> anyhow::Result<()> {
    let (table, symbol) = stream_target(exchange, stream)
        .ok_or_else(|| anyhow::anyhow!("unmapped stream {stream} on {exchange}"))?;

    let prefix = schema::bronze_ws_prefix(exchange, stream, date);
    let keys = list_keys(store, &prefix).await?;

    let mut events = Vec::new();
    let mut rejects = Vec::new();
    let mut lines_total = 0usize;
    for key in &keys {
        let gz = get_bytes(store, key).await?;
        let mut raw = String::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut raw)?;
        for (i, line) in raw.lines().enumerate() {
            lines_total += 1;
            let line_no = i as i32;
            let parsed: Result<BronzeLine, _> = serde_json::from_str(line);
            let Ok(bl) = parsed else {
                rejects.push(adapters::Reject {
                    src_file: key.clone(),
                    src_line: line_no,
                    reason: "bad bronze envelope".into(),
                });
                continue;
            };
            if bl.marker.is_some() {
                continue; // markers are for the gaps job, not silver
            }
            let Some(payload) = bl.payload else { continue };
            match parse_payload(exchange, &payload, Some(bl.ts_recv_ns), key, line_no) {
                Ok(evs) => events.extend(evs),
                Err(r) => rejects.push(r),
            }
        }
    }

    let written = match table {
        "trades" => {
            let mut seen = HashSet::new();
            let rows: Vec<_> = events
                .into_iter()
                .filter_map(|e| match e {
                    CanonicalEvent::Trade(t) => seen.insert(t.trade_id.clone()).then_some(t),
                    _ => None,
                })
                .collect();
            let n = rows.len();
            if n > 0 {
                let bytes = schema::write_parquet(&schema::trades_batch(rows)?)?;
                put_bytes(
                    store,
                    &schema::silver_key("trades", exchange, &symbol, date),
                    bytes,
                )
                .await?;
            }
            n
        }
        "book_top" => {
            let mut seen = HashSet::new();
            let rows: Vec<_> = events
                .into_iter()
                .filter_map(|e| match e {
                    CanonicalEvent::BookTop(b) => seen.insert(b.update_id).then_some(b),
                    _ => None,
                })
                .collect();
            let n = rows.len();
            if n > 0 {
                let bytes = schema::write_parquet(&schema::book_top_batch(rows)?)?;
                put_bytes(
                    store,
                    &schema::silver_key("book_top", exchange, &symbol, date),
                    bytes,
                )
                .await?;
            }
            n
        }
        "funding_rates" => {
            // Streamed predicted rates; ts_recv is the dedup axis (one
            // row per captured frame).
            let mut seen = HashSet::new();
            let rows: Vec<_> = events
                .into_iter()
                .filter_map(|e| match e {
                    CanonicalEvent::Funding(f) => seen.insert(f.ts_recv).then_some(f),
                    _ => None,
                })
                .collect();
            let n = rows.len();
            if n > 0 {
                let bytes = schema::write_parquet(&schema::funding_rates_batch(rows)?)?;
                put_bytes(
                    store,
                    &schema::funding_silver_key(exchange, &symbol, date, "predicted"),
                    bytes,
                )
                .await?;
            }
            n
        }
        other => anyhow::bail!("unknown table {other}"),
    };

    info!(
        exchange,
        stream,
        date,
        rows = written,
        lines = lines_total,
        "normalized"
    );
    handle_rejects(
        store,
        &format!("exchange={exchange}/stream={stream}/date={date}"),
        &rejects,
        lines_total,
    )
    .await
}
