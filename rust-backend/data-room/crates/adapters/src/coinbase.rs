//! Coinbase Exchange websocket adapter.
//!
//! Streams captured: `matches` (trades) and `ticker` (BBO). The feed's
//! `side` on a match is the MAKER side; canonical `side` is the AGGRESSOR
//! (taker), so it flips: maker sell = up-tick = taker buy.

use chrono::DateTime;
use data_room_schema::{BookTop, CanonicalEvent, Trade};
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "coinbase";

/// Coinbase product id ("BTC-USD") → our instrument id ("btc-usd.coinbase").
pub fn instrument_id(product_id: &str) -> String {
    format!("{}.{}", product_id.to_lowercase(), EXCHANGE)
}

/// Which bronze stream a raw feed message routes to, e.g.
/// `matches.BTC-USD`. `None` = control traffic (subscriptions, errors)
/// that the collector spools under the connection's `control` stream.
pub fn route(payload: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(rename = "type")]
        kind: String,
        product_id: Option<String>,
    }
    let h: Head = serde_json::from_str(payload).ok()?;
    let product = h.product_id?;
    match h.kind.as_str() {
        "match" | "last_match" => Some(format!("matches.{product}")),
        "ticker" => Some(format!("ticker.{product}")),
        "heartbeat" => Some(format!("heartbeat.{product}")),
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawMatch {
    trade_id: u64,
    time: String,
    product_id: String,
    size: String,
    price: String,
    /// Maker side.
    side: String,
}

#[derive(Deserialize)]
struct RawTicker {
    sequence: i64,
    product_id: String,
    time: Option<String>,
    best_bid: String,
    best_bid_size: String,
    best_ask: String,
    best_ask_size: String,
}

fn parse_rfc3339_ns(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok()?.timestamp_nanos_opt()
}

fn reject(src_file: &str, src_line: i32, reason: impl ToString) -> Reject {
    Reject {
        src_file: src_file.into(),
        src_line,
        reason: reason.to_string(),
    }
}

/// Parse one captured payload into canonical events. Heartbeats and
/// control messages parse to an empty vec (captured, nothing to
/// normalize); malformed payloads are rejects.
pub fn parse(
    payload: &str,
    ts_recv: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<CanonicalEvent>, Reject> {
    let kind = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .ok_or_else(|| reject(src_file, src_line, "no type field"))?;

    match kind.as_str() {
        "match" | "last_match" => {
            let m: RawMatch =
                serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
            let aggressor = match m.side.as_str() {
                "buy" => "sell", // maker bought → taker sold
                "sell" => "buy", // maker sold → taker bought
                other => return Err(reject(src_file, src_line, format!("bad side {other}"))),
            };
            Ok(vec![CanonicalEvent::Trade(Trade {
                ts_event: parse_rfc3339_ns(&m.time),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument_id(&m.product_id),
                price: m.price.parse().map_err(|e| reject(src_file, src_line, e))?,
                size: m.size.parse().map_err(|e| reject(src_file, src_line, e))?,
                side: Some(aggressor.into()),
                trade_id: m.trade_id.to_string(),
                src_file: src_file.into(),
                src_line,
            })])
        }
        "ticker" => {
            let t: RawTicker =
                serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
            let f = |s: &str| s.parse::<f64>().map_err(|e| reject(src_file, src_line, e));
            Ok(vec![CanonicalEvent::BookTop(BookTop {
                ts_event: t.time.as_deref().and_then(parse_rfc3339_ns),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument_id(&t.product_id),
                update_id: t.sequence,
                bid_px: f(&t.best_bid)?,
                bid_sz: f(&t.best_bid_size)?,
                ask_px: f(&t.best_ask)?,
                ask_sz: f(&t.best_ask_size)?,
                src_file: src_file.into(),
                src_line,
            })])
        }
        // Captured but not normalized.
        "heartbeat" | "subscriptions" | "status" | "error" => Ok(vec![]),
        other => Err(reject(src_file, src_line, format!("unknown type {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MATCH: &str = r#"{"type":"match","trade_id":1069822481,"maker_order_id":"a","taker_order_id":"b","side":"sell","size":"0.00000001","price":"63381.59","product_id":"BTC-USD","sequence":97636649000,"time":"2026-08-13T02:08:19.371387Z"}"#;
    const TICKER: &str = r#"{"type":"ticker","sequence":97636649001,"product_id":"BTC-USD","price":"63381.59","open_24h":"62000.00","volume_24h":"12000.5","low_24h":"61500.00","high_24h":"63999.00","volume_30d":"400000","best_bid":"63381.58","best_bid_size":"0.5","best_ask":"63381.60","best_ask_size":"0.25","side":"buy","time":"2026-08-13T02:08:19.371387Z","trade_id":1069822481,"last_size":"0.00000001"}"#;

    #[test]
    fn match_parses_and_flips_maker_side_to_aggressor() {
        let ev = parse(MATCH, Some(1), "f", 0).unwrap();
        let CanonicalEvent::Trade(t) = &ev[0] else {
            panic!("expected trade")
        };
        assert_eq!(t.side.as_deref(), Some("buy")); // maker sell → taker buy
        assert_eq!(t.price, 63381.59);
        assert_eq!(t.trade_id, "1069822481");
        assert_eq!(t.instrument_id, "btc-usd.coinbase");
        // 2026-08-13T02:08:19.371387Z
        assert_eq!(t.ts_event, Some(1_786_586_899_371_387_000));
        assert_eq!(t.ts_recv, Some(1));
    }

    #[test]
    fn ticker_parses_bbo() {
        let ev = parse(TICKER, Some(2), "f", 1).unwrap();
        let CanonicalEvent::BookTop(b) = &ev[0] else {
            panic!("expected book_top")
        };
        assert_eq!(b.bid_px, 63381.58);
        assert_eq!(b.ask_px, 63381.60);
        assert_eq!(b.update_id, 97636649001);
    }

    #[test]
    fn heartbeat_and_control_are_empty_not_rejects() {
        let hb = r#"{"type":"heartbeat","sequence":90,"last_trade_id":20,"product_id":"BTC-USD","time":"2026-08-13T02:08:20.000000Z"}"#;
        assert!(parse(hb, Some(1), "f", 0).unwrap().is_empty());
        let subs = r#"{"type":"subscriptions","channels":[]}"#;
        assert!(parse(subs, Some(1), "f", 0).unwrap().is_empty());
    }

    #[test]
    fn garbage_is_a_reject() {
        assert!(parse("not json", Some(1), "f", 7).is_err());
        assert!(parse(
            r#"{"type":"match","trade_id":"not-a-number"}"#,
            Some(1),
            "f",
            7
        )
        .is_err());
    }

    #[test]
    fn routing_matches_bronze_stream_layout() {
        assert_eq!(route(MATCH).as_deref(), Some("matches.BTC-USD"));
        assert_eq!(route(TICKER).as_deref(), Some("ticker.BTC-USD"));
        assert_eq!(route(r#"{"type":"subscriptions","channels":[]}"#), None);
    }
}
