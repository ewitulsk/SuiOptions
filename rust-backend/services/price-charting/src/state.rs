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
    /// Pool coin types — the mid sampler's devInspect needs them as type args.
    pub base_coin_type: String,
    pub quote_coin_type: String,
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
    /// Written by the watcher's discovery pass, read by ingestion + API.
    pub watched: RwLock<HashMap<String, PoolMeta>>,
    /// registry_id → metadata for every whitelisted hybrid-exchange market
    /// (discovered from the orderbook service). Kept separate from
    /// `watched`: the DeepBook discovery pass replaces that map wholesale,
    /// and the mid sampler dev-inspects every entry as a DeepBook pool.
    pub watched_exchange: RwLock<HashMap<String, PoolMeta>>,
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
            watched_exchange: RwLock::new(HashMap::new()),
            trades_tx,
            mids_tx,
        }
    }
}
