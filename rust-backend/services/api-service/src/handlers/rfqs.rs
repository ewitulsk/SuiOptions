//! `GET /rfqs` — RFQ auction list (vault-implementation-guide doc 05 §2).
//! The mm-bot polls `?status=open` as its discovery fallback; dashboards
//! read the full lifecycle. JIT GraphQL to the indexer, like every read.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use protocol_types::ids::ObjectId;

use crate::state::AppState;

#[derive(Deserialize)]
pub struct RfqsQuery {
    /// open | settled | expired_unsold.
    pub status: Option<String>,
    /// Origin filter — a vault id for coupled auctions.
    pub origin: Option<String>,
}

#[derive(Serialize)]
pub struct RfqDto {
    pub rfq_id: String,
    pub bucket_id: String,
    pub origin: String,
    pub amount_raw: String,
    pub reserve_premium_raw: String,
    pub deadline_ms: i64,
    pub best_premium_raw: Option<String>,
    pub best_bidder: Option<String>,
    pub status: String,
    pub winner: Option<String>,
    pub net_premium_raw: Option<String>,
    pub position_id: Option<String>,
}

#[derive(Serialize)]
pub struct RfqsResponse {
    pub rfqs: Vec<RfqDto>,
}

pub async fn list_rfqs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RfqsQuery>,
) -> Result<Json<RfqsResponse>, StatusCode> {
    if let Some(s) = q.status.as_deref() {
        if !matches!(s, "open" | "settled" | "expired_unsold") {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let origin = match q.origin.as_deref() {
        Some(o) => Some(ObjectId::from_hex(o).map_err(|_| StatusCode::BAD_REQUEST)?),
        None => None,
    };
    let rows = state
        .indexer
        .rfqs(q.status.as_deref(), origin)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer rfqs query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let rfqs = rows
        .into_iter()
        .map(|r| RfqDto {
            rfq_id: r.rfq_id.to_hex(),
            bucket_id: r.bucket_id.to_hex(),
            origin: r.origin.to_hex(),
            amount_raw: r.amount.to_string(),
            reserve_premium_raw: r.reserve_premium.to_string(),
            deadline_ms: r.deadline_ms as i64,
            best_premium_raw: r.best_premium.map(|v| v.to_string()),
            best_bidder: r.best_bidder.map(|a| a.to_hex()),
            status: r.status,
            winner: r.winner.map(|a| a.to_hex()),
            net_premium_raw: r.net_premium.map(|v| v.to_string()),
            position_id: r.position_id.map(|p| p.to_hex()),
        })
        .collect();
    Ok(Json(RfqsResponse { rfqs }))
}
