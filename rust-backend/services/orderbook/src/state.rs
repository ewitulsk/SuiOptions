use dashmap::DashMap;
use exchange_book::Book;
use exchange_types::{Market, SuiAddress};
use crate::config::DirectEscrowIds;
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
    /// Exchange package id, served on `/v1/markets` so chart/analytics
    /// consumers can build the `settlement::FillEvent` event filter without
    /// reading deployments themselves.
    pub exchange_package: String,
    /// Shared ingress `Whitelist` id (guarded launch, SO-384) from the
    /// standalone whitelist package, served on `/v1/markets` and in route
    /// skeletons so takers can build fill PTBs. `None` on records
    /// predating the standalone package.
    pub whitelist_id: Option<String>,
    /// Served markets, keyed by registry ID. Mutable at runtime (SO-416):
    /// discovered listings are inserted live via `add_market`, so a new
    /// option series trades without a restart.
    pub markets: DashMap<SuiAddress, Market>,
    /// Registries whose on-chain pause flag is set (PauseEvent mirror);
    /// intake rejects instead of burning settlement attempts.
    pub paused: DashMap<SuiAddress, ()>,
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
    /// Direct-vault-escrow ids (SO-372): the exchange_adapter package and
    /// the trading-vault IntegrationRegistry, advertised to takers building
    /// `fill_vault_order(_reverse)` PTBs. `None` disables direct escrow.
    pub direct_escrow: Option<DirectEscrowIds>,
}

impl AppState {
    pub fn market(&self, registry_id: &SuiAddress) -> Option<Market> {
        self.markets.get(registry_id).map(|m| m.clone())
    }

    /// Resolve a market by registry hex, or by symbol as a convenience.
    pub fn resolve_market(&self, key: &str) -> Option<Market> {
        if let Ok(addr) = SuiAddress::parse(key) {
            if let Some(m) = self.market(&addr) {
                return Some(m);
            }
        }
        self.markets
            .iter()
            .find(|m| m.symbol.eq_ignore_ascii_case(key))
            .map(|m| m.clone())
    }

    /// Stable snapshot for `/v1/markets` and route planning.
    pub fn markets_snapshot(&self) -> Vec<Market> {
        let mut out: Vec<Market> = self.markets.iter().map(|m| m.clone()).collect();
        out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        out
    }

    /// Serve a market that appeared at runtime (discovered listing): insert
    /// the market and an empty book. Idempotent — an existing registry is
    /// left untouched (its book may hold live orders).
    pub fn add_market(&self, market: Market) -> bool {
        if self.markets.contains_key(&market.registry_id) {
            return false;
        }
        self.books
            .entry(market.registry_id)
            .or_insert_with(|| Arc::new(Mutex::new(Book::new(market.clone()))));
        self.markets.insert(market.registry_id, market);
        true
    }

    pub fn set_paused(&self, registry_id: &SuiAddress, paused: bool) {
        if paused {
            self.paused.insert(*registry_id, ());
        } else {
            self.paused.remove(registry_id);
        }
    }

    pub fn is_paused(&self, registry_id: &SuiAddress) -> bool {
        self.paused.contains_key(registry_id)
    }

    pub fn set_market_fee(&self, registry_id: &SuiAddress, fee_bps: u64) {
        if let Some(mut m) = self.markets.get_mut(registry_id) {
            m.current_fee_bps = fee_bps;
        }
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
