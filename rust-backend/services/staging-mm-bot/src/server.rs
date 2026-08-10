//! Ops server: `/health` (503 "starting" → 200 "ok", the deploy-gate
//! contract) and `/metrics` (Prometheus text, ungated). Same shape as
//! mm-bot's server minus the desk endpoints.

use std::net::SocketAddr;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use observability::ops::Readiness;
use tower_http::cors::{Any, CorsLayer};

pub fn spawn(addr: SocketAddr, readiness: Readiness) {
    tokio::spawn(async move {
        if let Err(e) = serve(addr, readiness).await {
            tracing::error!(error = %format!("{e:#}"), "ops server exited");
        }
    });
}

async fn serve(addr: SocketAddr, readiness: Readiness) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "ops server listening (/health, /metrics)");
    axum::serve(listener, app(readiness)).await?;
    Ok(())
}

fn app(readiness: Readiness) -> Router {
    Router::new()
        .route("/health", get(move || health(readiness.clone())))
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(observability::middleware::http_obs))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any))
}

async fn health(readiness: Readiness) -> Response {
    if readiness.is_ready() {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "starting").into_response()
    }
}
