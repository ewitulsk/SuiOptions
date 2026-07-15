//! axum HTTP server (internal-only — never proxied by nginx).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tracing::info;

use crate::handlers;
use crate::state::AppState;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/accounts", get(handlers::accounts))
        .route("/tweets", post(handlers::post_tweet))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "twitter-service http listening");
    axum::serve(listener, app).await?;
    Ok(())
}
