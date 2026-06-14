//! Read API for the predicted-APY snapshots.
//!
//! `GET /vault-apy/:vault_id` → `{ vault_id, predicted: [...] }`. api-service
//! composes this with the indexer's realized series into `/vaults/:id/apy`.
//! Kept separate from the ops server (`/health`, `/metrics`) so Prometheus and
//! the read path don't share a port's concerns.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tracing::{error, info};

use crate::db::DbPool;

#[derive(Serialize)]
struct PredictedPoint {
    t_ms: i64,
    apy: f64,
    /// `current` | `forecast`.
    kind: String,
    horizon: i32,
    confidence: f64,
}

#[derive(Serialize)]
struct VaultApyResponse {
    vault_id: String,
    predicted: Vec<PredictedPoint>,
}

/// Spawn the read-API server as a background task.
pub fn spawn(addr: SocketAddr, pool: Arc<DbPool>) {
    tokio::spawn(async move {
        let app = Router::new()
            .route("/vault-apy/:vault_id", get(get_vault_apy))
            .with_state(pool);
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                info!(%addr, "read API listening (/vault-apy/:vault_id)");
                if let Err(e) = axum::serve(listener, app).await {
                    error!(error = %e, "read API server exited");
                }
            }
            Err(e) => error!(error = %e, %addr, "read API failed to bind"),
        }
    });
}

async fn get_vault_apy(
    State(pool): State<Arc<DbPool>>,
    Path(vault_id): Path<String>,
) -> Result<Json<VaultApyResponse>, StatusCode> {
    let vid = vault_id.clone();
    let rows = tokio::task::spawn_blocking(move || crate::db::predictions_for(&pool, &vid))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| {
            error!(error = %e, "predictions read failed");
            StatusCode::BAD_GATEWAY
        })?;
    let predicted = rows
        .into_iter()
        .map(|r| PredictedPoint {
            t_ms: r.t_ms,
            apy: r.apy,
            kind: r.kind,
            horizon: r.horizon,
            confidence: r.confidence,
        })
        .collect();
    Ok(Json(VaultApyResponse { vault_id, predicted }))
}
