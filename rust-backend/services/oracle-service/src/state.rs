use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use oracle_client::WsMessage;
use pyth_client::{BenchmarkVol, PriceCache, PriceFeedId};
use tokio::sync::broadcast;

/// Shared handler state. Cheap to `Arc`-clone; every field is internally shared.
pub struct AppState {
    /// Live prices, written by the fanout drain loop from the one SSE stream.
    pub price_cache: PriceCache,
    /// Cached + paced realized-vol client, shared across every `/vol/realized`
    /// request so its `(feed, day)` cache and request pacer span all callers.
    pub benchmark_vol: Arc<BenchmarkVol>,
    /// Fanout hub: the drain loop publishes price/status frames; each `/ws`
    /// connection subscribes a receiver.
    pub fanout: broadcast::Sender<WsMessage>,
    /// Feeds discovered from the token-info catalog at boot (what `/prices`
    /// enumerates and the SSE subscription covers).
    pub feeds: Vec<PriceFeedId>,
    /// Whether the upstream Hermes stream is currently healthy.
    pub upstream_healthy: Arc<AtomicBool>,
}
