//! Drains the one Pyth SSE stream and, in a single place, keeps the price cache
//! current, broadcasts each update to WS subscribers, and tracks upstream
//! health. Replaces `PriceCache::spawn_updater` so the cache and the fanout
//! never diverge.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Alert when the data plane goes quiet (SO-354).
///
/// The hermes-beta outage proved connection health is not delivery
/// health: the SSE session stayed "healthy" while publishing nothing for
/// 41 hours and no alert fired. This watchdog watches the fanout itself —
/// the one point every upstream (Pyth SSE, crossbar poller) flows
/// through — and raises a tagged error once deliveries stop, which the
/// alert-id Grafana rule turns into a page with no extra infra.
///
/// Also exports `price_data_plane_age_seconds` for dashboards.
pub fn spawn_stale_watchdog(mut rx: broadcast::Receiver<WsMessage>) {
    /// Quiet time before the first alert. Well past any poll cadence or
    /// SSE reconnect, far under the 5s/10s consumer gates' worth caring
    /// about — by the time this fires, quoting has already stopped.
    const STALE_ALERT_AFTER: Duration = Duration::from_secs(60);
    /// Re-fire cadence while the outage persists (keeps the Grafana
    /// count-over-time rule firing without log spam).
    const REFIRE_EVERY: Duration = Duration::from_secs(60);

    tokio::spawn(async move {
        // Boot counts as fresh: a service that never receives a first
        // price still alerts one threshold later.
        let mut last_price = Instant::now();
        let mut last_fire: Option<Instant> = None;
        loop {
            match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
                Ok(Ok(WsMessage::Price { .. })) => last_price = Instant::now(),
                Ok(Ok(WsMessage::Status { .. })) => {}
                // Lagged means prices are flowing faster than we read —
                // that IS delivery.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => last_price = Instant::now(),
                Ok(Err(broadcast::error::RecvError::Closed)) => return,
                Err(_timeout) => {}
            }
            let age = last_price.elapsed();
            metrics::gauge!("price_data_plane_age_seconds").set(age.as_secs_f64());
            if age >= STALE_ALERT_AFTER && last_fire.is_none_or(|t| t.elapsed() >= REFIRE_EVERY) {
                last_fire = Some(Instant::now());
                tracing::error!(
                    alert_id = "price-data-plane-stale",
                    age_secs = age.as_secs(),
                    "no price updates reaching the fanout — every consumer's staleness gate is (or will be) rejecting quotes"
                );
            }
        }
    });
}
