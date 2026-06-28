//! `GET /rfqs` — RFQ auction list (vault-implementation-guide doc 05 §2).
//! The mm-bot polls `?status=open` as its discovery fallback; dashboards
//! read the full lifecycle. JIT GraphQL to the indexer, like every read.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
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
    /// Option kind: `call` | `put`. Defaults to `call`, so existing callers
    /// (and the call mm-bot) keep seeing call auctions unchanged. The put
    /// bidder passes `?kind=put`.
    pub kind: Option<String>,
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
    /// Premium before the protocol RFQ fee (settled auctions only).
    pub gross_premium_raw: Option<String>,
    /// Protocol RFQ fee taken at settle (settled auctions only).
    pub fee_raw: Option<String>,
    /// `"call"` | `"put"` — the option kind this auction is for.
    pub option_kind: String,
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
    // Default to "call" so pre-puts callers are unaffected.
    let kind = q.kind.as_deref().unwrap_or("call");
    if !matches!(kind, "call" | "put") {
        return Err(StatusCode::BAD_REQUEST);
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
        // TODO(cash-secured-puts): source the per-row kind from the indexer's
        // `optionKind` once `indexer_graphql::Rfq` carries it. Until then every
        // auction is a call, so `?kind=put` correctly yields no rows and
        // `?kind=call` (the default) returns everything — back-compat-safe.
        .map(|r| (rfq_kind_of(&r), r))
        .filter(|(rfq_kind, _)| *rfq_kind == kind)
        .map(|(rfq_kind, r)| RfqDto {
            option_kind: rfq_kind.to_string(),
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
            gross_premium_raw: r.gross_premium.map(|v| v.to_string()),
            fee_raw: r.fee.map(|v| v.to_string()),
        })
        .collect();
    Ok(Json(RfqsResponse { rfqs }))
}

/// The option kind of one auction. Defaults to `"call"`; once
/// `indexer_graphql::Rfq` exposes `option_kind` this should read it.
fn rfq_kind_of(_r: &indexer_graphql::Rfq) -> &'static str {
    // TODO(cash-secured-puts): return `r.option_kind` (mapped to a static str)
    // once the indexer-graphql `Rfq` carries the field.
    "call"
}

/// One bid in an auction's history, ascending by sequence.
#[derive(Serialize)]
pub struct RfqBidDto {
    pub sequence: i64,
    pub bidder: String,
    pub call_recipient: String,
    pub premium_raw: String,
}

#[derive(Serialize)]
pub struct RfqBidsResponse {
    pub rfq_id: String,
    pub bids: Vec<RfqBidDto>,
}

/// `GET /rfqs/:rfq_id/bids` — the ascending bid history for one auction. The
/// vault track record joins this per round (origin = vault id → its RFQs →
/// their bids).
pub async fn list_rfq_bids(
    State(state): State<Arc<AppState>>,
    Path(rfq_id): Path<String>,
) -> Result<Json<RfqBidsResponse>, StatusCode> {
    let id = ObjectId::from_hex(&rfq_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let bids = state.indexer.rfq_bids(id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer rfq_bids query failed");
        StatusCode::BAD_GATEWAY
    })?;
    let bids = bids
        .into_iter()
        .map(|b| RfqBidDto {
            sequence: b.sequence as i64,
            bidder: b.bidder.to_hex(),
            call_recipient: b.call_recipient.to_hex(),
            premium_raw: b.premium.to_string(),
        })
        .collect();
    Ok(Json(RfqBidsResponse { rfq_id, bids }))
}
