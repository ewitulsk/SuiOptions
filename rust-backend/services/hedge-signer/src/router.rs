//! axum HTTP server.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::bluefin_proxy::{self, BluefinProxy};
use crate::state::{AppState, FrostState};
use crate::{frost_handlers, handlers};

pub async fn serve(
    addr: SocketAddr,
    state: Arc<AppState>,
    frost_state: Arc<FrostState>,
    proxy: Arc<BluefinProxy>,
    allowed_origins: &[String],
) -> Result<()> {
    let cors = build_cors(allowed_origins)?;

    // NOTE: `.layer(cors)` wraps every route merged BEFORE it — the /frost
    // and /bluefin surfaces the browser dashboard calls are covered.
    let app = Router::new()
        .route("/health", get(handlers::health))
        .route("/pubkey", get(handlers::pubkey))
        .route("/policy", get(handlers::policy))
        .route("/sign", post(handlers::sign))
        .with_state(state)
        .merge(frost_router(frost_state))
        .merge(bluefin_proxy::router(proxy))
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

/// The FROST threshold-signing surface (keygen + two-round signing +
/// group-pubkey lookup), on its own state — no signing key needed.
pub fn frost_router(state: Arc<FrostState>) -> Router {
    Router::new()
        .route("/frost/pubkey/:vault_id", get(frost_handlers::pubkey))
        .route(
            "/frost/registration/:vault_id",
            get(frost_handlers::registration),
        )
        .route("/frost/keygen/round1", post(frost_handlers::keygen_round1))
        .route("/frost/keygen/round2", post(frost_handlers::keygen_round2))
        .route("/frost/sign/round1", post(frost_handlers::sign_round1))
        .route("/frost/sign/round2", post(frost_handlers::sign_round2))
        .with_state(state)
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
