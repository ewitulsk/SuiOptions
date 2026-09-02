//! Bluefin Pro market-data websocket adapter (SO-404).
//!
//! Public feed on `wss://stream.api.sui-prod.bluefin.io/ws/market` — no
//! auth, no API key. Frames are `{"event": …, "payload": {…}}` and every
//! market payload carries its own `symbol`, so routing needs only the
//! envelope.
//!
//! Streams captured (subscribe names are the SDK's `MarketDataStreamName`
//! values, used verbatim as collector.toml `channels`):
//!   `Diff_Depth_200_ms` -> OrderbookDiffDepthUpdate -> `book.{symbol}`
//!   `Recent_Trade`      -> RecentTradesUpdates      -> `trades.{symbol}`
//!   `Ticker`            -> TickerUpdate             -> `ticker.{symbol}`
//!
//! 200 ms is the deliberate choice over the 10 ms variant: ~20x the bronze
//! volume for no gain at our decision horizon (see sui-collection-plan §1.2).
//!
//! Funding (SO-446): the REST `GET /v1/exchange/fundingRateHistory`
//! poller lands in `funding.{symbol}` and parses to settled rows
//! (`parse_funding_history`); the ticker stream's `nextFundingTimeAtMillis`
//! rollovers are the live-observed settlement clock (`parse_ticker`,
//! consumed by `normalizer::bluefin_funding`). Settlements are hourly —
//! verified against the history endpoint, 3600 s apart.
//!
//! Depth (SO-446, S5 → `book_l2`): `parse_book` turns
//! `OrderbookDiffDepthUpdate` frames into absolute level rows sequenced
//! by `firstUpdateId..lastUpdateId`; `parse_depth_rest` turns the
//! `GET /v1/exchange/depth` poller's response (`depth.{symbol}`) into a
//! full-book snapshot at `lastUpdateId` — the documented sync point for
//! the diff stream. `OrderbookPartialDepthUpdate` frames are top-N only
//! (5/10/20 levels), so they are NOT snapshots and are skipped.
//! All numeric fields arrive as E9-scaled integer strings.

use data_room_schema::{BookL2, FundingRate};
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "bluefin";
/// Bluefin Pro settles funding hourly (fundingRateHistory rows are 3600 s
/// apart; the ticker's nextFundingTimeAtMillis sits on the hour).
pub const FUNDING_INTERVAL_HOURS: f64 = 1.0;

/// Venue symbol ("SUI-PERP") → our instrument id ("sui-perp.bluefin").
pub fn instrument_id(symbol: &str) -> String {
    format!("{}.{}", symbol.to_lowercase(), EXCHANGE)
}

/// Silver `symbol=` partition value: the venue symbol verbatim
/// ("SUI-PERP"), which already matches the Hyperliquid convention.
pub fn partition_symbol(symbol: &str) -> String {
    symbol.to_uppercase()
}

fn reject(src_file: &str, src_line: i32, reason: impl ToString) -> Reject {
    Reject {
        src_file: src_file.into(),
        src_line,
        reason: reason.to_string(),
    }
}

/// E9 integer string → f64. Strict: a non-integer string is a reject,
/// because a venue format change should surface, not round.
fn e9(s: &str) -> Option<f64> {
    s.parse::<i64>().ok().map(|v| v as f64 / 1e9)
}

#[derive(Deserialize)]
struct FundingHistoryRow {
    symbol: String,
    #[serde(rename = "fundingRateE9")]
    funding_rate_e9: String,
    #[serde(rename = "fundingTimeAtMillis")]
    funding_time_ms: i64,
}

/// One `fundingRateHistory` response (newest first) → settled rows.
/// `ts_recv` is the poll's capture time: we observed the record, not
/// the settlement.
pub fn parse_funding_history(
    payload: &str,
    ts_recv: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<FundingRate>, Reject> {
    let rows: Vec<FundingHistoryRow> =
        serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    rows.into_iter()
        .map(|r| {
            Ok(FundingRate {
                ts_event: Some(r.funding_time_ms * 1_000_000),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument_id(&r.symbol),
                rate: e9(&r.funding_rate_e9).ok_or_else(|| {
                    reject(
                        src_file,
                        src_line,
                        format!("bad fundingRateE9 {}", r.funding_rate_e9),
                    )
                })?,
                interval_hours: FUNDING_INTERVAL_HOURS,
                kind: "settled".into(),
                mark_price: None,
                index_price: None,
                src_file: src_file.into(),
                src_line,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct RawDepth {
    symbol: String,
    #[serde(rename = "updatedAtMillis")]
    updated_ms: i64,
    #[serde(rename = "bidsE9")]
    bids_e9: Vec<[String; 2]>,
    #[serde(rename = "asksE9")]
    asks_e9: Vec<[String; 2]>,
    #[serde(rename = "firstUpdateId")]
    first_update_id: Option<i64>,
    #[serde(rename = "lastUpdateId")]
    last_update_id: i64,
}

fn depth_rows(
    d: RawDepth,
    is_snapshot: bool,
    ts_recv: i64,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<BookL2>, Reject> {
    let instrument = instrument_id(&d.symbol);
    let mut rows = Vec::with_capacity(d.bids_e9.len() + d.asks_e9.len());
    for (side, levels) in [("bid", &d.bids_e9), ("ask", &d.asks_e9)] {
        for [px, sz] in levels {
            let p =
                |v: &str| e9(v).ok_or_else(|| reject(src_file, src_line, format!("bad level {v}")));
            rows.push(BookL2 {
                // The REST snapshot reports 0 here; that is "unknown".
                ts_event: (d.updated_ms > 0).then_some(d.updated_ms * 1_000_000),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument.clone(),
                seq: d.last_update_id,
                seq_first: if is_snapshot { None } else { d.first_update_id },
                is_snapshot,
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

/// One captured `book.{symbol}` frame → level rows. Diffs carry absolute
/// sizes (0 = level removed) at `firstUpdateId..lastUpdateId`; partial
/// depth frames parse to nothing (top-N is not a book image); anything
/// else on the stream is a reject.
pub fn parse_book(
    payload: &str,
    ts_recv: i64,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<BookL2>, Reject> {
    #[derive(Deserialize)]
    struct Frame {
        event: String,
        payload: Option<serde_json::Value>,
    }
    let f: Frame = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    match (f.event.as_str(), f.payload) {
        ("OrderbookDiffDepthUpdate", Some(raw)) => {
            let d: RawDepth =
                serde_json::from_value(raw).map_err(|e| reject(src_file, src_line, e))?;
            if d.first_update_id.is_none() {
                return Err(reject(src_file, src_line, "diff without firstUpdateId"));
            }
            depth_rows(d, false, ts_recv, src_file, src_line)
        }
        ("OrderbookPartialDepthUpdate", Some(_)) => Ok(vec![]),
        (ev, _) => Err(reject(
            src_file,
            src_line,
            format!("not a depth frame: {ev}"),
        )),
    }
}

/// One captured `GET /v1/exchange/depth` response → a full-book snapshot
/// at `lastUpdateId` (`is_snapshot = true`).
pub fn parse_depth_rest(
    payload: &str,
    ts_recv: i64,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<BookL2>, Reject> {
    let d: RawDepth = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    depth_rows(d, true, ts_recv, src_file, src_line)
}

/// The funding-relevant slice of a `TickerUpdate` frame.
#[derive(Debug, Clone, PartialEq)]
pub struct TickerFunding {
    pub symbol: String,
    /// Scheduled next settlement, ms epoch — on the hour.
    pub next_funding_ms: i64,
    /// `lastFundingRateE9` as a per-interval rate.
    pub last_rate: f64,
    pub mark_price: f64,
    pub oracle_price: f64,
}

/// Parse one captured ticker frame. Anything but a `TickerUpdate` on the
/// ticker stream is a reject.
pub fn parse_ticker(payload: &str, src_file: &str, src_line: i32) -> Result<TickerFunding, Reject> {
    #[derive(Deserialize)]
    struct Frame {
        event: String,
        payload: Option<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct RawTicker {
        symbol: String,
        #[serde(rename = "lastFundingRateE9")]
        last_funding_rate_e9: String,
        #[serde(rename = "nextFundingTimeAtMillis")]
        next_funding_ms: i64,
        #[serde(rename = "markPriceE9")]
        mark_price_e9: String,
        #[serde(rename = "oraclePriceE9")]
        oracle_price_e9: String,
    }
    let f: Frame = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    let (Some(raw), "TickerUpdate") = (f.payload, f.event.as_str()) else {
        return Err(reject(
            src_file,
            src_line,
            format!("not a ticker: {}", f.event),
        ));
    };
    let t: RawTicker = serde_json::from_value(raw).map_err(|e| reject(src_file, src_line, e))?;
    let p = |name: &str, v: &str| {
        e9(v).ok_or_else(|| reject(src_file, src_line, format!("bad {name} {v}")))
    };
    Ok(TickerFunding {
        symbol: t.symbol,
        next_funding_ms: t.next_funding_ms,
        last_rate: p("lastFundingRateE9", &t.last_funding_rate_e9)?,
        mark_price: p("markPriceE9", &t.mark_price_e9)?,
        oracle_price: p("oraclePriceE9", &t.oracle_price_e9)?,
    })
}

/// Which bronze stream a frame routes to. `None` = control traffic
/// (subscription acks, errors, and the event types we do not subscribe to).
pub fn route(payload: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Frame {
        event: String,
        payload: Option<serde_json::Value>,
    }
    let f: Frame = serde_json::from_str(payload).ok()?;
    let data = f.payload?;

    // RecentTradesUpdates carries a list; the others are a single object.
    let symbol = match data.get("symbol").and_then(|s| s.as_str()) {
        Some(s) => s.to_string(),
        None => data
            .get("trades")
            .and_then(|t| t.get(0))
            .or_else(|| data.get(0))?
            .get("symbol")?
            .as_str()?
            .to_string(),
    };

    match f.event.as_str() {
        "OrderbookDiffDepthUpdate" | "OrderbookPartialDepthUpdate" => {
            Some(format!("book.{symbol}"))
        }
        "RecentTradesUpdates" => Some(format!("trades.{symbol}")),
        "TickerUpdate" => Some(format!("ticker.{symbol}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured off the live mainnet socket.
    const BOOK: &str = include_str!("../fixtures/bluefin-diffdepth.json");
    const TICKER: &str = include_str!("../fixtures/bluefin-ticker.json");
    /// Captured off the live socket too, from production bronze. SUI-PERP
    /// is quiet enough that this took ~1.7 h of capture to record a single
    /// print — one 12-SUI fill — which is itself the answer to doc 07
    /// open question #2.
    const TRADES: &str = include_str!("../fixtures/bluefin-trades.json");

    #[test]
    fn real_frames_route() {
        assert_eq!(route(BOOK).as_deref(), Some("book.SUI-PERP"));
        assert_eq!(route(TRADES).as_deref(), Some("trades.SUI-PERP"));
        assert_eq!(route(TICKER).as_deref(), Some("ticker.SUI-PERP"));
    }

    #[test]
    fn control_and_garbage_route_to_none() {
        // Subscription ack: no payload.
        assert_eq!(route(r#"{"event":"SubscriptionAck"}"#), None);
        // Subscribed-to-nothing / unknown event with a payload.
        assert_eq!(
            route(r#"{"event":"MarkPriceUpdate","payload":{"symbol":"SUI-PERP"}}"#),
            None
        );
        assert_eq!(route("not json"), None);
        assert_eq!(route("{}"), None);
    }

    /// Real `fundingRateHistory?symbol=SUI-PERP&limit=3` response,
    /// captured 2026-09-01.
    const FUNDING: &str = include_str!("../fixtures/bluefin-fundingRateHistory.json");

    #[test]
    fn real_funding_history_parses_as_settled() {
        let rows = parse_funding_history(FUNDING, Some(5), "f", 2).unwrap();
        assert_eq!(rows.len(), 3);
        let r = &rows[0];
        assert_eq!(r.instrument_id, "sui-perp.bluefin");
        assert_eq!(r.kind, "settled");
        assert_eq!(r.rate, 0.0000125); // "12500" E9
        assert_eq!(r.interval_hours, 1.0);
        assert_eq!(r.ts_event, Some(1_788_325_206_288 * 1_000_000));
        assert_eq!((r.ts_recv, r.src_line), (Some(5), 2));
        assert_eq!((r.mark_price, r.index_price), (None, None));
        // Hourly cadence, newest first.
        assert_eq!(
            rows[0].ts_event.unwrap() - rows[1].ts_event.unwrap(),
            3_599_810 * 1_000_000
        );
    }

    #[test]
    fn real_ticker_frame_yields_funding_clock() {
        let t = parse_ticker(TICKER, "f", 0).unwrap();
        assert_eq!(t.symbol, "SUI-PERP");
        assert_eq!(t.next_funding_ms, 1_786_824_000_000); // 2026-08-15T20:00:00Z
        assert_eq!(t.next_funding_ms % 3_600_000, 0);
        assert_eq!(t.last_rate, 0.0000125);
        assert_eq!(t.mark_price, 0.6798);
        assert_eq!(t.oracle_price, 0.6804);
    }

    #[test]
    fn funding_bad_shapes_are_rejects() {
        assert!(parse_ticker(BOOK, "f", 0)
            .unwrap_err()
            .reason
            .contains("not a ticker"));
        assert!(parse_ticker("{}", "f", 0).is_err());
        let decimal = r#"[{"fundingRateE9":"1.5","fundingTimeAtMillis":1,"symbol":"SUI-PERP"}]"#;
        assert!(parse_funding_history(decimal, None, "f", 0)
            .unwrap_err()
            .reason
            .contains("bad fundingRateE9"));
        assert_eq!(parse_funding_history("[]", None, "f", 0).unwrap(), vec![]);
    }

    #[test]
    fn ids_and_partitions() {
        assert_eq!(instrument_id("SUI-PERP"), "sui-perp.bluefin");
        assert_eq!(partition_symbol("SUI-PERP"), "SUI-PERP");
    }

    /// Real frames off the live socket, 2026-09-01: a multi-level diff and
    /// a `Partial_Depth_20` frame; plus a real `/v1/exchange/depth`
    /// response from the same minute.
    const BOOK_MULTI: &str = include_str!("../fixtures/bluefin-diffdepth-multi.json");
    const PARTIAL: &str = include_str!("../fixtures/bluefin-partialdepth.json");
    const DEPTH_REST: &str = include_str!("../fixtures/bluefin-depth-rest.json");

    #[test]
    fn real_diff_frames_parse_as_absolute_levels() {
        let rows = parse_book(BOOK, 9, "f", 4).unwrap();
        assert_eq!(rows.len(), 1); // one bid, no asks in this frame
        let r = &rows[0];
        assert_eq!(r.instrument_id, "sui-perp.bluefin");
        assert_eq!((r.side.as_str(), r.price, r.size), ("bid", 0.6787, 2.0));
        assert_eq!((r.seq_first, r.seq), (Some(16_837_928), 16_837_928));
        assert!(!r.is_snapshot);
        assert_eq!(r.ts_event, Some(1_786_822_333_604 * 1_000_000));
        assert_eq!((r.ts_recv, r.src_line), (9, 4));

        let rows = parse_book(BOOK_MULTI, 9, "f", 0).unwrap();
        assert_eq!(rows.len(), 12); // 6 bids + 6 asks
        assert!(rows
            .iter()
            .all(|r| r.seq_first == Some(67_638_374) && r.seq == 67_638_385));
        assert_eq!(rows.iter().filter(|r| r.side == "bid").count(), 6);
    }

    #[test]
    fn partial_depth_is_not_a_snapshot_and_control_is_a_reject() {
        assert_eq!(parse_book(PARTIAL, 1, "f", 0).unwrap(), vec![]);
        assert!(parse_book(TICKER, 1, "f", 0)
            .unwrap_err()
            .reason
            .contains("not a depth frame"));
        assert!(parse_book(r#"{"event":"SubscriptionAck"}"#, 1, "f", 0).is_err());
    }

    #[test]
    fn real_rest_depth_is_a_full_snapshot() {
        let rows = parse_depth_rest(DEPTH_REST, 3, "f", 0).unwrap();
        assert_eq!(rows.len(), 49 + 48); // whole visible book that minute
        assert!(rows
            .iter()
            .all(|r| r.is_snapshot && r.seq_first.is_none() && r.seq == 67_639_469));
        // updatedAtMillis is 0 on the REST response → unknown, not epoch.
        assert!(rows.iter().all(|r| r.ts_event.is_none()));
        let bid = &rows[0];
        assert_eq!(
            (bid.side.as_str(), bid.price, bid.size),
            ("bid", 0.7202, 69.0)
        );
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
        assert!(parse_depth_rest("{}", 1, "f", 0).is_err());
    }

    /// The depth frame is the one whose shape we actually depend on: the
    /// E9 string encoding and the update-id pair are what a future
    /// `book_deltas` replay needs, so pin them here even though nothing
    /// parses them yet.
    #[test]
    fn depth_frame_carries_what_p4_will_need() {
        let v: serde_json::Value = serde_json::from_str(BOOK).unwrap();
        let p = &v["payload"];
        assert!(p["firstUpdateId"].is_number());
        assert!(p["lastUpdateId"].is_number());
        assert!(p["updatedAtMillis"].is_number());
        // E9-scaled decimal strings, not floats.
        for side in ["bidsE9", "asksE9"] {
            if let Some(level) = p[side].get(0) {
                assert!(level[0].is_string(), "{side} price must be a string");
                assert!(level[1].is_string(), "{side} size must be a string");
            }
        }
    }
}
