//! Last-known-price cache, written to by the SSE task and read from the
//! RFQ hot path.
//!
//! Two notions of staleness live here:
//!   - `observed_at` — local monotonic clock; how long since we last
//!     received any update for this feed. Catches a wedged or
//!     disconnected stream.
//!   - `publish_time_ms` — Pyth's publisher timestamp; how old the
//!     market data is from Pyth's perspective. Catches the case where
//!     the stream is technically alive but Pyth itself isn't publishing.
//!
//! [`PriceCache::get_fresh`] requires both to be inside their respective
//! bounds before handing the price back.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::stream::StreamEvent;
use super::types::PriceFeedId;

#[derive(Clone, Debug)]
pub struct CachedPrice {
    pub price: f64,
    pub conf: f64,
    pub publish_time_ms: i64,
    pub observed_at: Instant,
}

#[derive(Clone, Default)]
pub struct PriceCache {
    inner: Arc<DashMap<PriceFeedId, CachedPrice>>,
}

impl PriceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the latest cached price for `id`, or `None` if we've never
    /// seen one, the local observation is older than `max_local_age`, or
    /// the publisher-side timestamp is older than `max_publish_lag`.
    pub fn get_fresh(
        &self,
        id: PriceFeedId,
        max_local_age: Duration,
        max_publish_lag: Duration,
    ) -> Option<CachedPrice> {
        let entry = self.inner.get(&id)?;
        let cp = entry.clone();
        drop(entry);
        if cp.observed_at.elapsed() > max_local_age {
            return None;
        }
        let now_ms = now_unix_ms();
        let lag_ms = (now_ms as i64).saturating_sub(cp.publish_time_ms);
        if lag_ms > max_publish_lag.as_millis() as i64 {
            return None;
        }
        Some(cp)
    }

    /// Latest cached price ignoring staleness — useful for diagnostics or
    /// the vol sampler, which already operates on a coarse cadence.
    pub fn peek(&self, id: PriceFeedId) -> Option<CachedPrice> {
        self.inner.get(&id).map(|e| e.clone())
    }

    /// Directly seed/overwrite a feed's cached entry. Used by test harnesses
    /// and the price-quote simulator that don't run the real SSE task.
    pub fn insert(&self, id: PriceFeedId, cp: CachedPrice) {
        self.inner.insert(id, cp);
    }

    /// Spawn the background task that drains `rx` (from
    /// [`super::stream::spawn_subscriber`]) into this cache. Logs
    /// connection state transitions at info level.
    pub fn spawn_updater(&self, mut rx: mpsc::Receiver<StreamEvent>) -> JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::Price(upd) => {
                        let Ok(id) = upd.feed_id() else { continue };
                        let (Ok(price), Ok(conf)) = (upd.price.price_f64(), upd.price.conf_f64())
                        else {
                            continue;
                        };
                        let cp = CachedPrice {
                            price,
                            conf,
                            publish_time_ms: upd.price.publish_time_ms(),
                            observed_at: Instant::now(),
                        };
                        debug!(%id, price, conf, publish_time_ms = cp.publish_time_ms, "pyth cache update");
                        inner.insert(id, cp);
                    }
                    StreamEvent::Disconnected { reason } => {
                        warn!(reason, "pyth stream disconnected; cached prices will age out");
                    }
                    StreamEvent::Reconnected => {
                        info!("pyth stream reconnected");
                    }
                }
            }
            debug!("pyth cache updater: channel closed, exiting");
        })
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
