//! axum HTTP server. Proxied by nginx at /<env>/social-bot/ — Slack and
//! Discord deliver their signed webhooks through it.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tracing::info;

use crate::state::AppState;
use crate::{discord, slack};

async fn health() -> &'static str {
    "ok"
}

pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/slack/command", post(slack::command))
        .route("/discord/interactions", post(discord::interactions))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "social-bot http listening");
    axum::serve(listener, app).await?;
    Ok(())
}
