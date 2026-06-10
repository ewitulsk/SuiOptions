//! Two axum routers on two ports.
//!
//! - [`serve_public`] — read API (open) + JWT-gated mutate routes, proxied by
//!   nginx (internet-facing). The mutate routes (`POST/PUT/DELETE /tokens`)
//!   require an admin JWT; token-info doesn't validate it itself — the
//!   `auth-client` middleware delegates to auth-service's internal `/verify`.
//! - [`serve_internal`] — the same mutate API with NO auth, bound on a
//!   separate port that nginx never proxies. Network isolation (VPC / docker
//!   network / VPN) is the only gate there, by design — kept for in-cluster
//!   callers (seeding, ops tools).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use auth_client::AuthClient;

use crate::handlers::{orgs, tokens};
use crate::state::AppState;

/// Public API: open reads + JWT-gated writes. `auth` points at auth-service's
/// internal verify route.
pub fn public_router(
    state: Arc<AppState>,
    allowed_origins: &[String],
    auth: Arc<AuthClient>,
) -> Result<Router> {
    let cors = build_cors(allowed_origins)?;

    // Mutating routes, gated by the admin-JWT middleware. Same handlers the
    // internal router exposes.
    let writes = Router::new()
        .route("/tokens", post(tokens::create_token))
        .route(
            "/tokens/:coin_type",
            put(tokens::update_token).delete(tokens::delete_token),
        )
        .route("/orgs", post(orgs::create_org))
        .route(
            "/orgs/:org_id",
            put(orgs::update_org).delete(orgs::delete_org),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            auth,
            auth_client::require_auth,
        ));

    let reads = Router::new()
        .route("/health", get(health))
        .route("/tokens", get(tokens::list_tokens))
        .route("/tokens/:coin_type", get(tokens::get_token))
        .route("/orgs", get(orgs::list_orgs))
        .route("/orgs/:org_id", get(orgs::get_org))
        .route("/package-info", get(tokens::package_info));

    Ok(reads.merge(writes).with_state(state).layer(cors))
}

/// Mutating internal API. No CORS layer — not browser-facing.
pub fn internal_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tokens", post(tokens::create_token))
        .route("/tokens/:coin_type", put(tokens::update_token))
        .route("/tokens/:coin_type", delete(tokens::delete_token))
        .route("/orgs", post(orgs::create_org))
        .route("/orgs/:org_id", put(orgs::update_org))
        .route("/orgs/:org_id", delete(orgs::delete_org))
        .with_state(state)
}

pub async fn serve_public(
    addr: SocketAddr,
    state: Arc<AppState>,
    allowed_origins: &[String],
    auth: Arc<AuthClient>,
) -> Result<()> {
    let app = public_router(state, allowed_origins, auth)?;
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
