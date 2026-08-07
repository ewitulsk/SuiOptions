use dashmap::DashMap;
use exchange_book::Book;
use exchange_types::{Market, SuiAddress};
use crate::settlement::MatchJob;
use crate::db::Db;
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// One message on the WS fanout. Clients subscribe to channels
/// (`book.{market}`, `trades.{market}`, `orders.{addr}`) and the per-socket
/// task filters this global stream.
#[derive(Clone, Debug)]
pub struct WsMsg {
    pub channel: String,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug)]
pub struct IntakeConfig {
    /// Reject orders expiring sooner than now + this (§5.4: 30s).
    pub min_ttl_ms: u64,
    /// Reject orders expiring later than now + this.
    pub max_ttl_ms: u64,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        IntakeConfig { min_ttl_ms: 30_000, max_ttl_ms: 24 * 60 * 60 * 1000 }
    }
}

pub struct AppState {
    pub markets: Vec<Market>,
    /// Per-market books, keyed by registry ID. A `Mutex<Book>` per market is
    /// the v1 stand-in for the single-writer task; contention stays
    /// per-market either way.
    pub books: DashMap<SuiAddress, Arc<Mutex<Book>>>,
    pub db: Db,
    /// Match intents resolved into jobs for the settlement submitter.
    pub match_tx: mpsc::Sender<MatchJob>,
    /// Global WS fanout.
    pub ws_tx: broadcast::Sender<WsMsg>,
    pub intake: IntakeConfig,
}

impl AppState {
    pub fn market(&self, registry_id: &SuiAddress) -> Option<&Market> {
        self.markets.iter().find(|m| m.registry_id == *registry_id)
    }

    /// Resolve a market by registry hex, or by symbol as a convenience.
    pub fn resolve_market(&self, key: &str) -> Option<&Market> {
        if let Ok(addr) = SuiAddress::parse(key) {
            if let Some(m) = self.market(&addr) {
                return Some(m);
            }
        }
        self.markets
            .iter()
            .find(|m| m.symbol.eq_ignore_ascii_case(key))
    }

    pub fn book(&self, registry_id: &SuiAddress) -> Option<Arc<Mutex<Book>>> {
        self.books.get(registry_id).map(|b| b.clone())
    }

    pub fn publish(&self, channel: impl Into<String>, payload: Value) {
        let _ = self.ws_tx.send(WsMsg { channel: channel.into(), payload });
    }

    pub fn publish_book_snapshot(&self, market: &Market) {
        if let Some(book) = self.book(&market.registry_id) {
            let (bids, asks) = book.lock().snapshot(50);
            self.publish(
                format!("book.{}", market.registry_id.to_hex()),
                serde_json::json!({
                    "type": "snapshot",
                    "market": market.registry_id.to_hex(),
                    "bids": bids,
                    "asks": asks,
                }),
            );
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis() as u64
}
