//! axum HTTP server. Routes are added here.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::state::AppState;

#[derive(Serialize)]
pub struct BucketDto {
    pub bucket_id: String,
    pub asset_type: String,
    pub settlement_type: String,
    /// Stringified to avoid JSON precision loss on u64.
    pub strike: String,
    pub expiry_ms: String,
    pub total_written: String,
    pub exercise_cursor: String,
}

#[derive(Serialize)]
pub struct BucketsResponse {
    pub buckets: Vec<BucketDto>,
}

pub async fn serve(
    addr: SocketAddr,
    state: Arc<AppState>,
    allowed_origins: &[String],
) -> Result<()> {
    let cors = build_cors(allowed_origins)?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/buckets", get(list_buckets))
        .with_state(state)
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "api-service http listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn list_buckets(State(state): State<Arc<AppState>>) -> Json<BucketsResponse> {
    let buckets = state
        .active_buckets()
        .into_iter()
        .map(|(id, b)| BucketDto {
            bucket_id: id.to_hex(),
            asset_type: b.asset_type.as_str().to_string(),
            settlement_type: b.settlement_type.as_str().to_string(),
            strike: b.strike.to_string(),
            expiry_ms: b.expiry_ms.to_string(),
            total_written: b.total_written.to_string(),
            exercise_cursor: b.exercise_cursor.to_string(),
        })
        .collect();
    Json(BucketsResponse { buckets })
}

fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(Any)
        .allow_headers(Any))
}
