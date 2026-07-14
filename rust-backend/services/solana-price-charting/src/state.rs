//! Shared service state.
//!
//! The broadcast channels + watched-pool map exist for the future ingestion
//! task (there is no Solana order-book source yet): the WS layer already
//! subscribes to them, so when ingestion lands it only needs to write rows
//! and send on these channels — the serving side is done.

use std::collections::HashMap;

use parking_lot::RwLock;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::db::repo::Repo;

/// Immutable-per-pool metadata a future discovery pass resolves. Mints are
/// base58 SPL mint addresses (compared byte-exact).
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub bucket_id: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub base_mint: String,
    pub quote_mint: String,
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

/// One order-book midpoint sample, fanned out to WS subscribers as it lands.
#[derive(Debug, Clone, Serialize)]
pub struct MidMsg {
    pub pool_id: String,
    pub ts_ms: i64,
    /// Midpoint in display units (quote per base).
    pub mid: f64,
}

pub struct AppState {
    pub repo: Repo,
    /// pool_id → metadata for every pool currently in the tradeable set.
    /// Nothing writes it yet (no ingestion source); read by the API.
    pub watched: RwLock<HashMap<String, PoolMeta>>,
    /// Live fill fan-out for the WS layer.
    pub trades_tx: broadcast::Sender<TradeMsg>,
    /// Live midpoint fan-out for the WS layer.
    pub mids_tx: broadcast::Sender<MidMsg>,
}

impl AppState {
    pub fn new(repo: Repo) -> Self {
        let (trades_tx, _) = broadcast::channel(1024);
        let (mids_tx, _) = broadcast::channel(1024);
        Self {
            repo,
            watched: RwLock::new(HashMap::new()),
            trades_tx,
            mids_tx,
        }
    }
}
