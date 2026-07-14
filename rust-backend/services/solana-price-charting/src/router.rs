//! HTTP + WS API. Wire shapes are byte-compatible with the Sui twin's so the
//! frontend chart layer is drop-in.
//!
//! - `GET /health` — liveness.
//! - `GET /pools` — pools with last price / 24h volume (empty until a Solana
//!   order-book ingestion source lands).
//! - `GET /bars?pool_id&interval&from_ms&to_ms` — carry-forward OHLC plus
//!   the order-book midpoint line on the same grid, capped at 1000 bars
//!   (the range is clamped, newest-first priority).
//! - `GET /price-at?pool_id&ms` — last known mid/close at or before `ms`.
//! - `GET /vault-apy/:vault_id` — latest predicted-APY snapshot for a vault;
//!   solana-api-service composes it with the indexer's realized series.
//! - `GET /ws` — subscribe `{"type":"subscribe","pool_id":…,"interval":…}`;
//!   the server pushes `{"type":"bar",…}` / `{"type":"mid",…}` when
//!   ingestion broadcasts (currently never — clients already handle quiet
//!   streams).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, info, warn};

use crate::bars::{
    align_down, carry_forward, carry_forward_mids, Bar, Interval, MidPoint, SampledMid, TradedBar,
};
use crate::db::repo::Repo;
use crate::state::{AppState, MidMsg, TradeMsg};

const MAX_BARS: i64 = 1_000;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>, allowed_origins: &[String]) -> Result<()> {
    let cors = build_cors(allowed_origins)?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/pools", get(list_pools))
        .route("/bars", get(get_bars))
        .route("/price-at", get(get_price_at))
        .route("/vault-apy/:vault_id", get(get_vault_apy))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "solana-price-charting listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new().allow_origin(origins).allow_methods(Any).allow_headers(Any))
}

// ---- /pools ----------------------------------------------------------------

#[derive(Serialize)]
struct PoolDto {
    pool_id: String,
    bucket_id: String,
    last_price: f64,
    last_trade_ms: i64,
    volume_24h_base: f64,
    /// Still in the tradeable set (false = winding down toward TTL).
    watched: bool,
}

async fn list_pools(State(state): State<Arc<AppState>>) -> Result<Json<Vec<PoolDto>>, StatusCode> {
    let repo = state.repo.clone();
    let rows = tokio::task::spawn_blocking(move || repo.pool_summaries())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(db_err)?;
    let watched = state.watched.read();
    Ok(Json(
        rows.into_iter()
            .map(|r| PoolDto {
                watched: watched.contains_key(&r.pool_id),
                pool_id: r.pool_id,
                bucket_id: r.bucket_id,
                last_price: r.last_price,
                last_trade_ms: r.last_trade_ms,
                volume_24h_base: r.volume_24h_base,
            })
            .collect(),
    ))
}

// ---- /bars -----------------------------------------------------------------

#[derive(Deserialize)]
struct BarsQuery {
    pool_id: String,
    interval: String,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

#[derive(Serialize)]
struct BarsResponse {
    bars: Vec<Bar>,
    /// Order-book midpoint line on the same interval grid.
    mids: Vec<MidPoint>,
}

async fn get_bars(
    State(state): State<Arc<AppState>>,
    Query(q): Query<BarsQuery>,
) -> Result<Json<BarsResponse>, StatusCode> {
    let interval = Interval::parse(&q.interval).ok_or(StatusCode::BAD_REQUEST)?;
    let now = chrono::Utc::now().timestamp_millis();
    let to = q.to_ms.unwrap_or(now).min(now + interval.ms());
    // Clamp the range so a missing/huge `from` serves the newest MAX_BARS.
    let min_from = to - MAX_BARS * interval.ms();
    let from = q.from_ms.unwrap_or(min_from).max(min_from);
    if from >= to {
        return Err(StatusCode::BAD_REQUEST);
    }

    let bars = compute_bars(&state.repo, &q.pool_id, interval, from, to)
        .await
        .map_err(db_err)?;
    let mids = compute_mids(&state.repo, &q.pool_id, interval, from, to)
        .await
        .map_err(db_err)?;
    Ok(Json(BarsResponse { bars, mids }))
}

async fn compute_bars(
    repo: &Repo,
    pool_id: &str,
    interval: Interval,
    from_ms: i64,
    to_ms: i64,
) -> anyhow::Result<Vec<Bar>> {
    let repo = repo.clone();
    let pool = pool_id.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Bar>> {
        let raw = repo.raw_bars(&pool, interval.secs(), from_ms, to_ms)?;
        let seed = repo.last_close_before(&pool, align_down(from_ms, interval.ms()))?;
        let traded: Vec<TradedBar> = raw
            .into_iter()
            .map(|r| TradedBar {
                t: r.bucket.timestamp_millis(),
                o: r.open,
                h: r.high,
                l: r.low,
                c: r.close,
                v: r.volume_base,
            })
            .collect();
        Ok(carry_forward(&traded, interval, from_ms, to_ms, seed))
    })
    .await?
}

async fn compute_mids(
    repo: &Repo,
    pool_id: &str,
    interval: Interval,
    from_ms: i64,
    to_ms: i64,
) -> anyhow::Result<Vec<MidPoint>> {
    let repo = repo.clone();
    let pool = pool_id.to_string();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<MidPoint>> {
        let raw = repo.raw_mids(&pool, interval.secs(), from_ms, to_ms)?;
        let seed = repo.last_mid_before(&pool, align_down(from_ms, interval.ms()))?;
        let sampled: Vec<SampledMid> = raw
            .into_iter()
            .map(|r| SampledMid { t: r.bucket.timestamp_millis(), m: r.mid })
            .collect();
        Ok(carry_forward_mids(&sampled, interval, from_ms, to_ms, seed))
    })
    .await?
}

// ---- /price-at -------------------------------------------------------------

#[derive(Deserialize)]
struct PriceAtQuery {
    pool_id: String,
    ms: i64,
}

/// Last known per-token price of a pool's base coin (the option token, quoted
/// in settlement) at or before `ms`. `mid` is the order-book midpoint (a
/// continuous fair value); `close` is the last traded price. Either may be
/// `null` if the pool had no book/trade data before `ms` — with no ingestion
/// source yet, both always are.
#[derive(Serialize)]
struct PriceAtResponse {
    pool_id: String,
    ms: i64,
    mid: Option<f64>,
    close: Option<f64>,
}

async fn get_price_at(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PriceAtQuery>,
) -> Result<Json<PriceAtResponse>, StatusCode> {
    let repo = state.repo.clone();
    let pool = q.pool_id.clone();
    let ms = q.ms;
    let (mid, close) = tokio::task::spawn_blocking(move || -> anyhow::Result<(Option<f64>, Option<f64>)> {
        Ok((repo.last_mid_before(&pool, ms)?, repo.last_close_before(&pool, ms)?))
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(db_err)?;
    Ok(Json(PriceAtResponse { pool_id: q.pool_id, ms: q.ms, mid, close }))
}

// ---- /vault-apy/:vault_id --------------------------------------------------

#[derive(Serialize)]
struct PredictedApyPoint {
    t_ms: i64,
    apy: f64,
    /// Low/high of the predicted range (straddles `apy`).
    apy_low: f64,
    apy_high: f64,
    /// Per-round probability the call is assigned.
    assignment_prob: f64,
    /// Per-round (not annualized) net yield if assigned — the downside scenario.
    downside_round_yield: f64,
    /// `current` | `forecast`.
    kind: String,
    horizon: i32,
    confidence: f64,
}

/// Latest predicted-APY snapshot for one vault. Same shape as the Sui twin's
/// (`{ vault_id, predicted: [...] }`) so a composing service reads it
/// unchanged. The realized series is left to the indexer; the persisted
/// realized history lives in `vault_realized_apy` for direct querying.
#[derive(Serialize)]
struct VaultApyResponse {
    vault_id: String,
    predicted: Vec<PredictedApyPoint>,
}

async fn get_vault_apy(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<VaultApyResponse>, StatusCode> {
    let repo = state.repo.clone();
    let vid = vault_id.clone();
    let rows = tokio::task::spawn_blocking(move || repo.latest_predicted_apy(&vid))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(db_err)?;
    let predicted = rows
        .into_iter()
        .map(|r| PredictedApyPoint {
            t_ms: r.t_ms,
            apy: r.apy,
            apy_low: r.apy_low,
            apy_high: r.apy_high,
            assignment_prob: r.assignment_prob,
            downside_round_yield: r.downside_round_yield,
            kind: r.kind,
            horizon: r.horizon,
            confidence: r.confidence,
        })
        .collect();
    Ok(Json(VaultApyResponse { vault_id, predicted }))
}

fn db_err(e: anyhow::Error) -> StatusCode {
    warn!(error = %format!("{e:#}"), "charts query failed");
    StatusCode::BAD_GATEWAY
}

// ---- /ws -------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Subscribe { pool_id: String, interval: String },
    Unsubscribe { pool_id: String, interval: String },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMsg<'a> {
    Bar {
        pool_id: &'a str,
        interval: &'a str,
        bar: Bar,
    },
    Mid {
        pool_id: &'a str,
        interval: &'a str,
        point: MidPoint,
    },
    Error {
        message: String,
    },
}

async fn ws_upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(state, socket))
}

async fn ws_session(state: Arc<AppState>, mut socket: WebSocket) {
    let mut subs: HashSet<(String, Interval)> = HashSet::new();
    let mut trades = state.trades_tx.subscribe();
    let mut mids = state.mids_tx.subscribe();
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(15));

    loop {
        tokio::select! {
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break };
                if let Message::Text(text) = msg {
                    match serde_json::from_str::<ClientMsg>(&text) {
                        Ok(ClientMsg::Subscribe { pool_id, interval }) => {
                            let Some(iv) = Interval::parse(&interval) else {
                                let _ = send_json(&mut socket, &ServerMsg::Error {
                                    message: format!("unknown interval {interval}"),
                                }).await;
                                continue;
                            };
                            // Snapshot the current bar immediately so the chart
                            // doesn't wait for the next fill.
                            if let Some(bar) = current_bar(&state.repo, &pool_id, iv).await {
                                let _ = send_json(&mut socket, &ServerMsg::Bar {
                                    pool_id: &pool_id, interval: &interval, bar,
                                }).await;
                            }
                            if subs.insert((pool_id, iv)) {
                                metrics::gauge!("solana_price_charting_ws_subscribers").increment(1.0);
                            }
                        }
                        Ok(ClientMsg::Unsubscribe { pool_id, interval }) => {
                            if let Some(iv) = Interval::parse(&interval) {
                                if subs.remove(&(pool_id, iv)) {
                                    metrics::gauge!("solana_price_charting_ws_subscribers").decrement(1.0);
                                }
                            }
                        }
                        Err(e) => {
                            let _ = send_json(&mut socket, &ServerMsg::Error {
                                message: format!("bad message: {e}"),
                            }).await;
                        }
                    }
                }
            }
            trade = trades.recv() => {
                match trade {
                    Ok(t) => {
                        if let Err(e) = push_updates(&state, &mut socket, &subs, &t).await {
                            debug!(error = %e, "ws push failed; closing session");
                            break;
                        }
                    }
                    // Lagged: fills outpaced this socket — resync on the next one.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            mid = mids.recv() => {
                match mid {
                    Ok(m) => {
                        if let Err(e) = push_mid(&mut socket, &subs, &m).await {
                            debug!(error = %e, "ws mid push failed; closing session");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
    metrics::gauge!("solana_price_charting_ws_subscribers").decrement(subs.len() as f64);
}

/// On a fill, re-serve the now-current bar to every matching subscription.
async fn push_updates(
    state: &Arc<AppState>,
    socket: &mut WebSocket,
    subs: &HashSet<(String, Interval)>,
    t: &TradeMsg,
) -> anyhow::Result<()> {
    for (pool, iv) in subs {
        if pool != &t.pool_id {
            continue;
        }
        if let Some(bar) = current_bar(&state.repo, pool, *iv).await {
            send_json(
                socket,
                &ServerMsg::Bar {
                    pool_id: pool,
                    interval: interval_name(*iv),
                    bar,
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// On a book sample, serve the now-current bucket's midpoint to every
/// matching subscription. The latest sample IS the bucket's `last(mid)`,
/// so no DB round-trip is needed.
async fn push_mid(
    socket: &mut WebSocket,
    subs: &HashSet<(String, Interval)>,
    m: &MidMsg,
) -> anyhow::Result<()> {
    for (pool, iv) in subs {
        if pool != &m.pool_id {
            continue;
        }
        send_json(
            socket,
            &ServerMsg::Mid {
                pool_id: pool,
                interval: interval_name(*iv),
                point: MidPoint { t: align_down(m.ts_ms, iv.ms()), m: m.mid },
            },
        )
        .await?;
    }
    Ok(())
}

/// The bar covering "now" for one pool/interval (carry-forward if no trade
/// landed in the current bucket yet).
async fn current_bar(repo: &Repo, pool_id: &str, interval: Interval) -> Option<Bar> {
    let now = chrono::Utc::now().timestamp_millis();
    let start = align_down(now, interval.ms());
    compute_bars(repo, pool_id, interval, start, start + interval.ms())
        .await
        .ok()
        .and_then(|bars| bars.into_iter().next())
}

fn interval_name(iv: Interval) -> &'static str {
    match iv {
        Interval::M1 => "1m",
        Interval::M5 => "5m",
        Interval::M15 => "15m",
        Interval::H1 => "1h",
        Interval::H4 => "4h",
        Interval::D1 => "1d",
    }
}

async fn send_json<T: Serialize>(socket: &mut WebSocket, msg: &T) -> anyhow::Result<()> {
    let text = serde_json::to_string(msg)?;
    socket.send(Message::Text(text)).await?;
    Ok(())
}
