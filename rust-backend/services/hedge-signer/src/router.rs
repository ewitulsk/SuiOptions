//! axum HTTP server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
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
        .route("/health", get(handlers::health))
        .route("/pubkey", get(handlers::pubkey))
        .route("/policy", get(handlers::policy))
        .route("/sign", post(handlers::sign))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "hedge-signer http listening");
    axum::serve(listener, app).await?;
    Ok(())
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
