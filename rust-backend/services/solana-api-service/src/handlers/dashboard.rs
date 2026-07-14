//! `POST /dashboard/positions` — enrich a wallet's written positions.
//!
//! The frontend reads the authoritative list of Position account pubkeys
//! straight from the wallet's owned accounts, then posts them here. We
//! query the indexer for the chain truth (bucket strike/expiry/cursor +
//! the provenance denormalized onto each position) and layer on the
//! catalog (symbol/decimals, USD strike). Ids the indexer doesn't know yet
//! are simply absent from the response — the frontend renders those
//! degraded rather than dropping them, so a write never silently
//! disappears.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::handlers::positions::{position_dto, PositionDto};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct EnrichRequest {
    /// Position account pubkeys (base58).
    pub position_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct EnrichResponse {
    pub positions: Vec<PositionDto>,
}

pub async fn enrich_positions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnrichRequest>,
) -> Json<EnrichResponse> {
    let rows = match state.indexer.positions(&req.position_ids).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "indexer position enrichment failed");
            return Json(EnrichResponse { positions: vec![] });
        }
    };

    let positions = rows
        .into_iter()
        .map(|p| position_dto(&state, p))
        .collect::<Vec<_>>();
    Json(EnrichResponse { positions })
}
