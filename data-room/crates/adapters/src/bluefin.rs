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
//! **Capture only.** L2 diffs want the `book_deltas` table, which is P4 in
//! the spec and does not exist — so there is no `parse()` here and no
//! `stream_target` arm in the ws normalizer. Bronze is sacred; the silver
//! layer replays it whenever P4 lands. Prices/sizes arrive as E9-scaled
//! decimal strings (`bidsE9`), which is a normalizer problem, not ours.

use serde::Deserialize;

pub const EXCHANGE: &str = "bluefin";

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
    /// Real venue trade objects (from `GET /v1/exchange/trades`) wrapped in
    /// the documented `RecentTradesUpdates` envelope. SUI-PERP prints only
    /// every few minutes at ~$1–10 a clip, so waiting for one on the socket
    /// was not worth blocking on — replace with a socket capture the first
    /// time bronze records one.
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
