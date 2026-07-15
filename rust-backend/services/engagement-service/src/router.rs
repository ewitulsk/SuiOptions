//! axum HTTP server (internal-only — never proxied by nginx).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::get;
use axum::Router;
use tracing::info;

use crate::handlers;
use crate::state::AppState;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/leaderboard", get(handlers::leaderboard))
        .route("/points/:handle", get(handlers::points))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "engagement-service http listening");
    axum::serve(listener, app).await?;
    Ok(())
}
