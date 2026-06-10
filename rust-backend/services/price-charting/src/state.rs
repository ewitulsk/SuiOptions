//! Shared service state.

use std::collections::HashMap;

use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::repo::Repo;

/// Immutable-per-pool metadata the watcher resolves from api-service.
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub bucket_id: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
}

/// One ingested fill, fanned out to WS subscribers as it lands.
#[derive(Debug, Clone, Serialize)]
pub struct TradeMsg {
    pub pool_id: String,
    pub ts_ms: i64,
    pub price: f64,
    /// Base volume in display units.
    pub base_qty: f64,
    pub taker_is_bid: bool,
}

pub struct AppState {
    pub repo: Repo,
    /// pool_id → metadata for every pool currently in the tradeable set.
    /// Written by the watcher's discovery pass, read by ingestion + API.
    pub watched: RwLock<HashMap<String, PoolMeta>>,
    /// Live fill fan-out for the WS layer.
    pub trades_tx: broadcast::Sender<TradeMsg>,
}

impl AppState {
    pub fn new(repo: Repo) -> Self {
        let (trades_tx, _) = broadcast::channel(1024);
        Self {
            repo,
            watched: RwLock::new(HashMap::new()),
            trades_tx,
        }
    }
}
