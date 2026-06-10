//! axum HTTP server. Routes are added here.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::handlers;
use crate::state::AppState;

pub async fn serve(
    addr: SocketAddr,
    state: Arc<AppState>,
    allowed_origins: &[String],
) -> Result<()> {
    let cors = build_cors(allowed_origins)?;

    let app = Router::new()
        .route("/health", get(health))
        .route("/buckets", get(handlers::buckets::list_buckets))
        .route("/orgs", get(handlers::orgs::list_orgs))
        .route("/buckets/:bucket_id", get(handlers::buckets::get_bucket))
        .route("/positions", get(handlers::positions::list_positions))
        .route(
            "/call-token-lots",
            get(handlers::call_token_lots::list_call_token_lots),
        )
        .route(
            "/dashboard/positions",
            post(handlers::dashboard::enrich_positions),
        )
        .route(
            "/indexer/progress",
            get(handlers::indexer_progress::get_progress),
        )
        .route("/events", get(handlers::events::list_events))
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
