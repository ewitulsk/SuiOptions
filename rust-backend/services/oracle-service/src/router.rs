//! axum router: REST snapshot + realized-vol endpoints and the `/ws` fanout.
//! Internal port only — never nginx-proxied.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use oracle_client::{PricePoint, PricesResponse, RealizedVolPoint, RealizedVolResponse, WsMessage};
use pyth_client::{benchmark_feed_id, PriceFeedId};
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::debug;

use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/prices", get(get_prices))
        .route("/snapshot", get(get_prices))
        .route("/prices/:feed", get(get_price))
        .route("/vol/realized", get(get_realized_vol))
        .route("/ws", get(ws_handler))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(observability::middleware::http_obs))
}

async fn health() -> &'static str {
    "ok"
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn point(feed: PriceFeedId, cp: &pyth_client::CachedPrice, now: i64) -> PricePoint {
    PricePoint {
        feed_id: feed.to_hex(),
        price: cp.price,
        conf: cp.conf,
        publish_time_ms: cp.publish_time_ms,
        age_ms: now - cp.publish_time_ms,
    }
}

async fn get_prices(State(state): State<Arc<AppState>>) -> Json<PricesResponse> {
    let now = now_ms();
    let prices = state
        .feeds
        .iter()
        .filter_map(|f| state.price_cache.peek(*f).map(|cp| point(*f, &cp, now)))
        .collect();
    Json(PricesResponse {
        as_of_ms: now,
        upstream_healthy: state.upstream_healthy.load(Ordering::Relaxed),
        prices,
    })
}

async fn get_price(
    State(state): State<Arc<AppState>>,
    Path(feed): Path<String>,
) -> Result<Json<PricePoint>, StatusCode> {
    let id = PriceFeedId::from_hex(&feed).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cp = state.price_cache.peek(id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(point(id, &cp, now_ms())))
}

#[derive(Debug, Deserialize)]
struct VolQuery {
    /// Comma-separated 64-hex feed ids.
    feeds: String,
    window_days: u32,
}

async fn get_realized_vol(
    State(state): State<Arc<AppState>>,
    Query(q): Query<VolQuery>,
) -> Result<Json<RealizedVolResponse>, StatusCode> {
    // Map each requested (beta) feed to its stable Benchmarks feed id here, so
    // consumers never have to know about the beta→stable mapping.
    let pairs: Vec<(PriceFeedId, PriceFeedId)> = q
        .feeds
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|tok| PriceFeedId::from_hex(tok).ok())
        .map(|orig| (orig, benchmark_feed_id(orig)))
        .collect();
    if pairs.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let stable_feeds: Vec<PriceFeedId> = pairs.iter().map(|(_, s)| *s).collect();
    let now_secs = now_ms() / 1000;
    let bulk = state
        .benchmark_vol
        .realized_sigma_bulk(&stable_feeds, q.window_days, now_secs)
        .await;

    let results = pairs
        .into_iter()
        .map(|(orig, stable)| {
            let (sigma, error) = match bulk.get(&stable) {
                Some(Ok(s)) => (Some(*s), None),
                Some(Err(e)) => (None, Some(format!("{e:#}"))),
                None => (None, Some("benchmark returned no result".to_string())),
            };
            RealizedVolPoint {
                feed_id: orig.to_hex(),
                stable_feed_id: stable.to_hex(),
                window_days: q.window_days,
                sigma,
                error,
            }
        })
        .collect();

    Ok(Json(RealizedVolResponse { results }))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.fanout.subscribe();

    // Lead with the current upstream status so a fresh subscriber knows whether
    // the prices it's about to receive are live.
    let initial = WsMessage::Status {
        upstream_healthy: state.upstream_healthy.load(Ordering::Relaxed),
        reason: None,
    };
    if let Ok(txt) = serde_json::to_string(&initial) {
        if socket.send(Message::Text(txt)).await.is_err() {
            return;
        }
    }

    loop {
        match rx.recv().await {
            Ok(msg) => {
                let Ok(txt) = serde_json::to_string(&msg) else {
                    continue;
                };
                if socket.send(Message::Text(txt)).await.is_err() {
                    break; // client gone
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                // A slow client missed some frames; the next price frames repopulate
                // its cache, so just keep going.
                debug!(skipped, "oracle ws subscriber lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
