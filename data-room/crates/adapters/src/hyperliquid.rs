//! Hyperliquid perps websocket adapter (SO-391).
//!
//! Public feed, no auth. Frames carry a `channel` field; the ones we
//! capture: `trades` (aggressor side "B" = buy / "A" = sell), `bbo`
//! (top-of-book, no venue sequence — `ts_recv` doubles as `update_id`),
//! `activeAssetCtx` (mark/oracle/funding/OI push, no venue timestamp).
//! Fixtures under fixtures/hl-*.json are real captured frames.

use schema::{BookTop, CanonicalEvent, FundingRate, Trade};
use serde::Deserialize;

use crate::Reject;

pub const EXCHANGE: &str = "hyperliquid";
/// Hyperliquid settles funding hourly.
pub const FUNDING_INTERVAL_HOURS: f64 = 1.0;

/// Coin ("BTC") → our instrument id ("btc-perp.hyperliquid").
pub fn instrument_id(coin: &str) -> String {
    format!("{}-perp.{}", coin.to_lowercase(), EXCHANGE)
}

/// Silver `symbol=` partition value: "BTC-PERP".
pub fn partition_symbol(coin: &str) -> String {
    format!("{}-PERP", coin.to_uppercase())
}

/// Which bronze stream a frame routes to. `None` = control traffic
/// (subscriptionResponse, pong, errors).
pub fn route(payload: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        channel: String,
        data: Option<serde_json::Value>,
    }
    let h: Head = serde_json::from_str(payload).ok()?;
    let data = h.data?;
    let coin = match h.channel.as_str() {
        "trades" => data.get(0)?.get("coin")?.as_str()?.to_string(),
        "bbo" | "activeAssetCtx" => data.get("coin")?.as_str()?.to_string(),
        _ => return None,
    };
    match h.channel.as_str() {
        "trades" => Some(format!("trades.{coin}")),
        "bbo" => Some(format!("bbo.{coin}")),
        "activeAssetCtx" => Some(format!("ctx.{coin}")),
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawTrade {
    coin: String,
    /// "B" bid/buy aggressor, "A" ask/sell aggressor.
    side: String,
    px: String,
    sz: String,
    /// ms epoch.
    time: i64,
    tid: u64,
}

#[derive(Deserialize)]
struct RawBbo {
    coin: String,
    /// ms epoch.
    time: i64,
    /// [bid, ask], each nullable.
    bbo: [Option<BboLevel>; 2],
}

#[derive(Deserialize)]
struct BboLevel {
    px: String,
    sz: String,
}

#[derive(Deserialize)]
struct RawCtxFrame {
    coin: String,
    ctx: RawCtx,
}

#[derive(Deserialize)]
struct RawCtx {
    /// Current hourly funding rate (predicted for the running hour).
    funding: String,
    #[serde(rename = "markPx")]
    mark_px: String,
    #[serde(rename = "oraclePx")]
    oracle_px: String,
}

fn reject(src_file: &str, src_line: i32, reason: impl ToString) -> Reject {
    Reject {
        src_file: src_file.into(),
        src_line,
        reason: reason.to_string(),
    }
}

/// Parse one captured frame into canonical events. Control frames parse
/// to an empty vec; malformed payloads are rejects.
pub fn parse(
    payload: &str,
    ts_recv: Option<i64>,
    src_file: &str,
    src_line: i32,
) -> Result<Vec<CanonicalEvent>, Reject> {
    #[derive(Deserialize)]
    struct Frame {
        channel: String,
        data: Option<serde_json::Value>,
    }
    let f: Frame = serde_json::from_str(payload).map_err(|e| reject(src_file, src_line, e))?;
    let Some(data) = f.data else {
        return Ok(vec![]);
    };

    match f.channel.as_str() {
        "trades" => {
            let trades: Vec<RawTrade> =
                serde_json::from_value(data).map_err(|e| reject(src_file, src_line, e))?;
            trades
                .into_iter()
                .map(|t| {
                    let side = match t.side.as_str() {
                        "B" => "buy",
                        "A" => "sell",
                        other => {
                            return Err(reject(src_file, src_line, format!("bad side {other}")))
                        }
                    };
                    Ok(CanonicalEvent::Trade(Trade {
                        ts_event: Some(t.time * 1_000_000),
                        ts_recv,
                        exchange: EXCHANGE.into(),
                        instrument_id: instrument_id(&t.coin),
                        price: t.px.parse().map_err(|e| reject(src_file, src_line, e))?,
                        size: t.sz.parse().map_err(|e| reject(src_file, src_line, e))?,
                        side: Some(side.into()),
                        trade_id: t.tid.to_string(),
                        src_file: src_file.into(),
                        src_line,
                    }))
                })
                .collect()
        }
        "bbo" => {
            let b: RawBbo =
                serde_json::from_value(data).map_err(|e| reject(src_file, src_line, e))?;
            let [Some(bid), Some(ask)] = b.bbo else {
                // One-sided book: capture-only, nothing to normalize.
                return Ok(vec![]);
            };
            let p = |s: &str| s.parse::<f64>().map_err(|e| reject(src_file, src_line, e));
            Ok(vec![CanonicalEvent::BookTop(BookTop {
                ts_event: Some(b.time * 1_000_000),
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument_id(&b.coin),
                // No venue sequence on this channel; capture time is the
                // unique, monotonic stand-in.
                update_id: ts_recv.unwrap_or(b.time * 1_000_000),
                bid_px: p(&bid.px)?,
                bid_sz: p(&bid.sz)?,
                ask_px: p(&ask.px)?,
                ask_sz: p(&ask.sz)?,
                src_file: src_file.into(),
                src_line,
            })])
        }
        "activeAssetCtx" => {
            let c: RawCtxFrame =
                serde_json::from_value(data).map_err(|e| reject(src_file, src_line, e))?;
            let p = |s: &str| s.parse::<f64>().map_err(|e| reject(src_file, src_line, e));
            Ok(vec![CanonicalEvent::Funding(FundingRate {
                ts_event: None, // frame carries no venue timestamp
                ts_recv,
                exchange: EXCHANGE.into(),
                instrument_id: instrument_id(&c.coin),
                rate: p(&c.ctx.funding)?,
                interval_hours: FUNDING_INTERVAL_HOURS,
                kind: "predicted".into(),
                mark_price: Some(p(&c.ctx.mark_px)?),
                index_price: Some(p(&c.ctx.oracle_px)?),
                src_file: src_file.into(),
                src_line,
            })])
        }
        // Control traffic: captured, not normalized.
        _ => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRADES: &str = include_str!("../fixtures/hl-trades.json");
    const BBO: &str = include_str!("../fixtures/hl-bbo.json");
    const CTX: &str = include_str!("../fixtures/hl-activeAssetCtx.json");

    #[test]
    fn real_trades_frame_parses() {
        let ev = parse(TRADES, Some(1), "f", 0).unwrap();
        assert!(!ev.is_empty());
        let CanonicalEvent::Trade(t) = &ev[0] else {
            panic!()
        };
        assert_eq!(t.instrument_id, "btc-perp.hyperliquid");
        assert_eq!(t.side.as_deref(), Some("sell")); // fixture side "A"
        assert_eq!(t.price, 63703.0);
        assert_eq!(t.ts_event, Some(1_786_629_709_190 * 1_000_000));
        assert_eq!(t.trade_id, "188164783631815");
        // Second trade in the frame is side "B" → buy.
        let CanonicalEvent::Trade(t2) = &ev[1] else {
            panic!()
        };
        assert_eq!(t2.side.as_deref(), Some("buy"));
    }

    #[test]
    fn real_bbo_frame_parses() {
        let ev = parse(BBO, Some(7), "f", 0).unwrap();
        let CanonicalEvent::BookTop(b) = &ev[0] else {
            panic!()
        };
        assert_eq!(b.bid_px, 63703.0);
        assert_eq!(b.ask_px, 63704.0);
        assert!(b.bid_px < b.ask_px);
        assert_eq!(b.update_id, 7); // ts_recv stand-in
    }

    #[test]
    fn real_ctx_frame_parses_as_predicted_funding() {
        let ev = parse(CTX, Some(9), "f", 0).unwrap();
        let CanonicalEvent::Funding(fr) = &ev[0] else {
            panic!()
        };
        assert_eq!(fr.kind, "predicted");
        assert_eq!(fr.rate, 0.0000125);
        assert_eq!(fr.interval_hours, 1.0);
        assert_eq!(fr.mark_price, Some(63703.0));
        assert_eq!(fr.index_price, Some(63730.0));
        assert_eq!(fr.ts_event, None);
    }

    #[test]
    fn routing_and_control() {
        assert_eq!(route(TRADES).as_deref(), Some("trades.BTC"));
        assert_eq!(route(BBO).as_deref(), Some("bbo.BTC"));
        assert_eq!(route(CTX).as_deref(), Some("ctx.BTC"));
        assert_eq!(route(r#"{"channel":"pong"}"#), None);
        assert_eq!(
            route(r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#),
            None
        );
        assert!(parse(r#"{"channel":"pong"}"#, Some(1), "f", 0)
            .unwrap()
            .is_empty());
    }
}
