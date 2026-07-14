//! `GET /indexer/progress` — proxy solana-indexer's slot-ingestion status
//! for the frontend Debug page. Returns 502 if the indexer is unreachable
//! so the page can render an "indexer unavailable" state rather than
//! hanging.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use crate::state::{AppState, Progress};

pub async fn get_progress(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Progress>, StatusCode> {
    match state.indexer.progress().await {
        Ok(progress) => Ok(Json(progress)),
        Err(e) => {
            tracing::warn!(error = %e, "indexer progress proxy failed");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}
