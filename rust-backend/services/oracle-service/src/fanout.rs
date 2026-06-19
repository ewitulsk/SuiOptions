//! Drains the one Pyth SSE stream and, in a single place, keeps the price cache
//! current, broadcasts each update to WS subscribers, and tracks upstream
//! health. Replaces `PriceCache::spawn_updater` so the cache and the fanout
//! never diverge.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use oracle_client::WsMessage;
use pyth_client::{CachedPrice, PriceCache, StreamEvent};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Consume `StreamEvent`s until the SSE task ends (it runs forever in practice,
/// reconnecting internally).
pub async fn run(
    mut rx: mpsc::Receiver<StreamEvent>,
    cache: PriceCache,
    fanout: broadcast::Sender<WsMessage>,
    upstream_healthy: Arc<AtomicBool>,
) {
    while let Some(evt) = rx.recv().await {
        match evt {
            StreamEvent::Price(upd) => {
                let Ok(id) = upd.feed_id() else { continue };
                let (Ok(price), Ok(conf)) = (upd.price.price_f64(), upd.price.conf_f64()) else {
                    continue;
                };
                let publish_time_ms = upd.price.publish_time_ms();
                cache.insert(
                    id,
                    CachedPrice {
                        price,
                        conf,
                        publish_time_ms,
                        observed_at: Instant::now(),
                    },
                );
                upstream_healthy.store(true, Ordering::Relaxed);
                // `send` errors only when there are zero subscribers — fine.
                let _ = fanout.send(WsMessage::Price {
                    feed_id: id.to_hex(),
                    price,
                    conf,
                    publish_time_ms,
                    sent_at_ms: now_unix_ms(),
                });
            }
            StreamEvent::Disconnected { reason } => {
                upstream_healthy.store(false, Ordering::Relaxed);
                warn!(%reason, "pyth upstream disconnected; clients will age out");
                let _ = fanout.send(WsMessage::Status {
                    upstream_healthy: false,
                    reason: Some(reason),
                });
            }
            StreamEvent::Reconnected => {
                upstream_healthy.store(true, Ordering::Relaxed);
                info!("pyth upstream reconnected");
                let _ = fanout.send(WsMessage::Status {
                    upstream_healthy: true,
                    reason: None,
                });
            }
        }
    }
    warn!("pyth SSE stream ended; fanout drain loop exiting");
}
