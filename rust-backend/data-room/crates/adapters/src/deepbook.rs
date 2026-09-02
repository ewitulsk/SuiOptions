//! DeepBook indexer adapter (SO-446, S5). Capture is a 30 s REST poll of
//! `GET /orderbook/{POOL}?level=2&depth=100` (50 levels/side, the
//! practical maximum) into `book.{POOL}`; every poll is a full snapshot,
//! so `is_snapshot` is always true and no reconstruction is needed.
//!
//! The indexer publishes no sequence number, so the response's own
//! `timestamp` (ms) is `seq`: it orders polls and dedups a cached
//! response served twice. Prices/sizes are already human decimal
//! strings. Fixture is a real response.

use data_room_schema::BookL2;
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "deepbook";

/// Pool ("SUI_USDC") → our instrument id ("sui-usdc.deepbook").
pub fn instrument_id(pool: &str) -> String {
    format!("{}.{}", pool.to_lowercase().replace('_', "-"), EXCHANGE)
}

/// Silver `symbol=` partition value ("SUI-USDC") for a pool ("SUI_USDC").
pub fn partition_symbol(pool: &str) -> String {
    pool.to_uppercase().replace('_', "-")
}

#[derive(Deserialize)]
struct Raw {
    /// ms epoch, as a string.
    timestamp: String,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

fn reject(src_file: &str, src_line: i32, reason: impl ToString) -> Reject {
    Reject {
        src_file: src_file.into(),
        src_line,
        reason: reason.to_string(),
    }
}

/// One captured snapshot → one row per level, all `is_snapshot = true`.
pub fn parse_book(
    payload: &str,
    pool: &str,
    ts_recv: i64,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<BookL2>, Reject> {
    let raw: Raw = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    let ts_ms: i64 = raw.timestamp.parse().map_err(|_| {
        reject(
            src_file,
            src_line,
            format!("bad timestamp {}", raw.timestamp),
        )
    })?;
    let instrument = instrument_id(pool);
    let mut rows = Vec::with_capacity(raw.bids.len() + raw.asks.len());
    for (side, levels) in [("bid", &raw.bids), ("ask", &raw.asks)] {
        for [px, sz] in levels {
            let p = |s: &str| {
                s.parse::<f64>()
                    .map_err(|_| reject(src_file, src_line, format!("bad level {s}")))
            };
            rows.push(BookL2 {
                ts_event: Some(ts_ms * 1_000_000),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument.clone(),
                seq: ts_ms,
                seq_first: None,
                is_snapshot: true,
                side: side.into(),
                price: p(px)?,
                size: p(sz)?,
                src_file: src_file.into(),
                src_line,
            });
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `orderbook/SUI_USDC?level=2&depth=100` response, 2026-09-01.
    const BOOK: &str = include_str!("../fixtures/deepbook-orderbook.json");

    #[test]
    fn real_snapshot_parses() {
        let rows = parse_book(BOOK, "SUI_USDC", 7, "f", 1).unwrap();
        assert_eq!(rows.len(), 100); // 50 bids + 50 asks
        let bid = &rows[0];
        assert_eq!(bid.instrument_id, "sui-usdc.deepbook");
        assert_eq!(
            (bid.side.as_str(), bid.price, bid.size),
            ("bid", 0.72305, 240.0)
        );
        assert_eq!(bid.seq, 1_788_325_441_259);
        assert_eq!(bid.ts_event, Some(1_788_325_441_259 * 1_000_000));
        assert!(bid.is_snapshot && bid.seq_first.is_none());
        assert_eq!((bid.ts_recv, bid.src_line), (7, 1));
        let ask = &rows[50];
        assert_eq!(
            (ask.side.as_str(), ask.price, ask.size),
            ("ask", 0.72324, 2076.0)
        );
        // Not crossed.
        let best_bid = rows
            .iter()
            .filter(|r| r.side == "bid")
            .map(|r| r.price)
            .fold(0.0, f64::max);
        let best_ask = rows
            .iter()
            .filter(|r| r.side == "ask")
            .map(|r| r.price)
            .fold(f64::MAX, f64::min);
        assert!(best_bid < best_ask);
    }

    #[test]
    fn bad_shapes_are_rejects() {
        assert!(parse_book("{}", "SUI_USDC", 1, "f", 0).is_err());
        assert!(parse_book(
            r#"{"timestamp":"x","bids":[],"asks":[]}"#,
            "SUI_USDC",
            1,
            "f",
            0
        )
        .unwrap_err()
        .reason
        .contains("bad timestamp"));
        assert!(parse_book(
            r#"{"timestamp":"1","bids":[["a","1"]],"asks":[]}"#,
            "SUI_USDC",
            1,
            "f",
            0
        )
        .unwrap_err()
        .reason
        .contains("bad level"));
        // The indexer's simulate_transaction error body is not a book.
        assert!(parse_book(
            "RPC error: No results from simulate_transaction",
            "SUI_USDC",
            1,
            "f",
            0
        )
        .is_err());
    }

    #[test]
    fn ids_and_partitions() {
        assert_eq!(instrument_id("SUI_USDC"), "sui-usdc.deepbook");
        assert_eq!(partition_symbol("DEEP_SUI"), "DEEP-SUI");
    }
}
