//! `GET /indexer/progress` — proxy the indexer's checkpoint-ingestion status
//! for the frontend Debug page (SO-107). Returns 502 if the indexer is
//! unreachable so the page can render an "indexer unavailable" state rather
//! than hanging.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::state::{AppState, IndexerProgress};

pub async fn get_progress(
    State(state): State<Arc<AppState>>,
) -> Result<Json<IndexerProgress>, StatusCode> {
    match state.query_indexer_progress().await {
        Ok(progress) => Ok(Json(progress)),
        Err(e) => {
            tracing::warn!(error = %e, "indexer progress proxy failed");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}
