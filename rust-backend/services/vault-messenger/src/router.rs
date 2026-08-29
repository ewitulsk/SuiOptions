//! HTTP API (mirrors cctp-relay's shape).
//!
//! - `GET /health` — liveness.
//! - `GET /messages?spoke_id=&status=&limit=&offset=` — paginated queue
//!   rows, lane order.
//! - `GET /lanes` — per-lane last seqs + queue depth + fee-pot report.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{status, MessageRow};
use crate::db::repo::blocking;
use crate::state::AppState;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>, allowed_origins: &[String]) -> Result<()> {
    let cors = build_cors(allowed_origins)?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/messages", get(list_messages))
        .route("/lanes", get(list_lanes))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "vault-messenger listening");
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

#[derive(Serialize)]
struct MessageDto {
    id: i64,
    direction: String,
    spoke_id: i64,
    seq: i64,
    msg_type: i16,
    status: String,
    attempts: i32,
    tx_hash: Option<String>,
    error: Option<String>,
    observed_tx: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl From<MessageRow> for MessageDto {
    fn from(r: MessageRow) -> Self {
        Self {
            id: r.id,
            direction: r.direction,
            spoke_id: r.spoke_id,
            seq: r.seq,
            msg_type: r.msg_type,
            status: r.status,
            attempts: r.attempts,
            tx_hash: r.tx_hash,
            error: r.error,
            observed_tx: r.observed_tx,
            created_at_ms: r.created_at.timestamp_millis(),
            updated_at_ms: r.updated_at.timestamp_millis(),
        }
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    spoke_id: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<MessageDto>>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let rows = blocking(&state.repo, move |r| {
        r.list_messages(q.spoke_id, q.status, limit, offset)
    })
    .await
    .map_err(internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[derive(Serialize)]
struct LaneDto {
    direction: String,
    spoke_id: i64,
    last_confirmed_seq: i64,
    pending: i64,
    submitted: i64,
    failed: i64,
}

#[derive(Serialize)]
struct LaneStatsDto {
    spoke_id: i64,
    fee_pot: String,
    last_state_sync_ms: i64,
}

#[derive(Serialize)]
struct LanesDto {
    lanes: Vec<LaneDto>,
    spokes: Vec<LaneStatsDto>,
}

async fn list_lanes(
    State(state): State<Arc<AppState>>,
) -> Result<Json<LanesDto>, (StatusCode, String)> {
    let out = blocking(&state.repo, |r| {
        let mut lanes = Vec::new();
        for (direction, spoke_id) in r.lanes()? {
            lanes.push(LaneDto {
                last_confirmed_seq: r.last_confirmed_seq(&direction, spoke_id)?,
                pending: r.count_with_status(&direction, spoke_id, status::PENDING)?,
                submitted: r.count_with_status(&direction, spoke_id, status::SUBMITTED)?,
                failed: r.count_with_status(&direction, spoke_id, status::FAILED)?,
                direction,
                spoke_id,
            });
        }
        let spokes = r
            .lane_stats()?
            .into_iter()
            .map(|s| LaneStatsDto {
                spoke_id: s.spoke_id,
                fee_pot: s.fee_pot.to_string(),
                last_state_sync_ms: s.last_state_sync_ms,
            })
            .collect();
        Ok(LanesDto { lanes, spokes })
    })
    .await
    .map_err(internal)?;
    Ok(Json(out))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
