//! Coinbase bronze (websocket JSONL) → silver trades + book_top for one
//! UTC day.

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

/// Discover this day's captured streams by listing the exchange prefix.
pub async fn streams_for_date(store: &Store, date: &str) -> anyhow::Result<Vec<String>> {
    let keys = list_keys(store, "bronze/v1/exchange=coinbase/").await?;
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
        .filter(|s| s.starts_with("matches.") || s.starts_with("ticker."))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    streams.sort();
    Ok(streams)
}

/// Normalize every stream captured on `date`. Returns partition count.
pub async fn normalize_day(store: &Store, date: &str) -> anyhow::Result<usize> {
    let streams = streams_for_date(store, date).await?;
    for stream in &streams {
        normalize_stream_day(store, stream, date).await?;
    }
    Ok(streams.len())
}

pub async fn normalize_stream_day(store: &Store, stream: &str, date: &str) -> anyhow::Result<()> {
    let prefix = schema::bronze_ws_prefix("coinbase", stream, date);
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
            match adapters::coinbase::parse(&payload, Some(bl.ts_recv_ns), key, line_no) {
                Ok(evs) => events.extend(evs),
                Err(r) => rejects.push(r),
            }
        }
    }

    let (symbol, is_matches) = {
        let (kind, product) = stream.split_once('.').unwrap_or((stream, ""));
        (product.to_string(), kind == "matches")
    };

    let written = if is_matches {
        let mut seen = HashSet::new();
        let trades: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e {
                CanonicalEvent::Trade(t) => seen.insert(t.trade_id.clone()).then_some(t),
                _ => None,
            })
            .collect();
        let n = trades.len();
        if n > 0 {
            let batch = schema::trades_batch(trades)?;
            let bytes = schema::write_parquet(&batch)?;
            put_bytes(
                store,
                &schema::silver_key("trades", "coinbase", &symbol, date),
                bytes,
            )
            .await?;
        }
        n
    } else {
        let mut seen = HashSet::new();
        let tops: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e {
                CanonicalEvent::BookTop(b) => seen.insert(b.update_id).then_some(b),
                _ => None,
            })
            .collect();
        let n = tops.len();
        if n > 0 {
            let batch = schema::book_top_batch(tops)?;
            let bytes = schema::write_parquet(&batch)?;
            put_bytes(
                store,
                &schema::silver_key("book_top", "coinbase", &symbol, date),
                bytes,
            )
            .await?;
        }
        n
    };

    info!(
        stream,
        date,
        rows = written,
        lines = lines_total,
        "normalized"
    );
    handle_rejects(
        store,
        &format!("exchange=coinbase/stream={stream}/date={date}"),
        &rejects,
        lines_total,
    )
    .await
}
