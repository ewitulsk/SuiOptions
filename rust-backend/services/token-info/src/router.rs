//! Two axum routers on two ports.
//!
//! - [`serve_public`] — read-only API, proxied by nginx (internet-facing).
//! - [`serve_internal`] — mutate API, bound on a separate port that nginx
//!   never proxies. Network isolation (VPC / docker network / VPN) is the only
//!   gate, by design.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::handlers::tokens;
use crate::state::AppState;

/// Read-only public API.
pub fn public_router(state: Arc<AppState>, allowed_origins: &[String]) -> Result<Router> {
    let cors = build_cors(allowed_origins)?;
    Ok(Router::new()
        .route("/health", get(health))
        .route("/tokens", get(tokens::list_tokens))
        .route("/tokens/:coin_type", get(tokens::get_token))
        .route("/package-info", get(tokens::package_info))
        .with_state(state)
        .layer(cors))
}

/// Mutating internal API. No CORS layer — not browser-facing.
pub fn internal_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tokens", post(tokens::create_token))
        .route("/tokens/:coin_type", put(tokens::update_token))
        .route("/tokens/:coin_type", delete(tokens::delete_token))
        .with_state(state)
}

pub async fn serve_public(
    addr: SocketAddr,
    state: Arc<AppState>,
    allowed_origins: &[String],
) -> Result<()> {
    let app = public_router(state, allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "token-info public API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn serve_internal(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = internal_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "token-info internal API listening");
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
