//! axum HTTP server. Proxied by nginx at /<env>/airdrop-bot/ — Discord
//! delivers its signed interaction webhooks through it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tracing::info;

use crate::discord;
use crate::state::AppState;

async fn health() -> &'static str {
    "ok"
}

pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/discord/interactions", post(discord::interactions))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "airdrop-bot http listening");
    axum::serve(listener, app).await?;
    Ok(())
}
