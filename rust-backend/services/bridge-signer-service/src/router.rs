//! Two axum routers on two ports (spec §5.3):
//! - [`serve_public`] (3000): `/health`, `/get_attestation`, `/group_keys`,
//!   `POST /sign_message`.
//! - [`serve_admin`] (3001, localhost-only in prod): Seal key-load + share
//!   provisioning + DKG — all stubbed at M1.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tracing::info;

use crate::handlers;
use crate::state::AppState;

pub fn public_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/get_attestation", get(handlers::get_attestation))
        .route("/group_keys", get(handlers::group_keys))
        .route("/sign_message", post(handlers::sign_message))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(observability::middleware::http_obs))
}

pub fn admin_router() -> Router {
    Router::new()
        .route("/health", get(handlers::health))
        .route("/admin/init_seal_key_load", post(handlers::admin_not_implemented))
        .route("/admin/complete_seal_key_load", post(handlers::admin_not_implemented))
        .route("/admin/provision_ecdsa_share", post(handlers::admin_not_implemented))
        .route("/admin/provision_ed25519_share", post(handlers::admin_not_implemented))
        .route("/admin/dkg/start", post(handlers::admin_not_implemented))
        .layer(axum::middleware::from_fn(observability::middleware::http_obs))
}

pub async fn serve_public(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "bridge-signer public API listening");
    axum::serve(listener, public_router(state)).await?;
    Ok(())
}

pub async fn serve_admin(addr: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "bridge-signer admin API listening");
    axum::serve(listener, admin_router()).await?;
    Ok(())
}
