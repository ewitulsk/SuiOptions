//! Two axum routers on two ports.
//!
//! - [`serve_public`] — login, registration and self-service, proxied by nginx.
//! - [`serve_internal`] — `/verify` and `/invites`, bound on a separate port
//!   nginx never proxies. Other services reach it container-to-container via
//!   `auth-client`. Minting an invite is unauthenticated, so keeping that port
//!   off the proxy IS the access control: anything that can reach it can mint
//!   an admin invite.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::handlers;
use crate::state::AppState;

pub fn public_router(state: Arc<AppState>, allowed_origins: &[String]) -> Result<Router> {
    let cors = build_cors(allowed_origins)?;
    Ok(Router::new()
        .route("/health", get(handlers::health))
        // Session.
        .route("/challenge", get(handlers::session::challenge))
        .route("/login", post(handlers::session::login))
        .route("/login/password", post(handlers::session::login_password))
        .route("/refresh", post(handlers::session::refresh))
        // Account lifecycle. `/me` and `/identities` authenticate inside the
        // handler rather than behind a route layer, since they need the account
        // row anyway and a layer would just fetch it twice.
        .route("/register", post(handlers::account::register))
        .route("/invites/preview", get(handlers::account::preview_invite))
        .route("/me", get(handlers::account::me))
        .route("/identities", post(handlers::account::add_identity))
        .route("/identities/:id", delete(handlers::account::remove_identity))
        .with_state(state)
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors))
}

pub fn internal_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::internal::ready))
        .route("/verify", post(handlers::internal::verify))
        .route("/invites", post(handlers::internal::create_invite))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
}

pub async fn serve_public(
    addr: SocketAddr,
    state: Arc<AppState>,
    allowed_origins: &[String],
) -> Result<()> {
    let app = public_router(state, allowed_origins)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "auth-service public API listening");
    // ConnectInfo so login/refresh can read the direct peer when there's no
    // X-Forwarded-For (local/dev).
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

pub async fn serve_internal(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = internal_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "auth-service internal API listening");
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
