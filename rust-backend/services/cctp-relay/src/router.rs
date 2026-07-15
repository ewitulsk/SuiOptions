//! HTTP API.
//!
//! - `GET /health` — liveness.
//! - `POST /transfers` — register a burn tx: `{tx_hash, origin_chain,
//!   wallet, destination_wallet?}`. Idempotent on (origin_chain, tx_hash).
//! - `GET /transfers?wallet=…&open=true` — transfers for the bridge page,
//!   with burn/attest/mint timestamps and computed duration_ms.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bigdecimal::ToPrimitive;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::db::models::{chain, status, NewTransfer, TransferRow};
use crate::state::AppState;

pub async fn serve(addr: SocketAddr, state: Arc<AppState>, allowed_origins: &[String]) -> Result<()> {
    let cors = build_cors(allowed_origins)?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/transfers", post(create_transfer).get(list_transfers))
        .with_state(state)
        .merge(observability::middleware::metrics_route())
        .layer(axum::middleware::from_fn(
            observability::middleware::http_obs,
        ))
        .layer(cors);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "cctp-relay listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors(allowed_origins: &[String]) -> Result<CorsLayer> {
    if allowed_origins.iter().any(|o| o == "*") {
        return Ok(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any));
    }
    let mut origins = Vec::with_capacity(allowed_origins.len());
    for o in allowed_origins {
        origins.push(o.parse()?);
    }
    Ok(CorsLayer::new().allow_origin(origins).allow_methods(Any).allow_headers(Any))
}

#[derive(Deserialize)]
struct CreateTransferReq {
    tx_hash: String,
    origin_chain: String,
    wallet: String,
    #[serde(default)]
    destination_wallet: Option<String>,
}

#[derive(Serialize)]
struct TransferDto {
    id: i64,
    origin_chain: String,
    origin_tx_hash: String,
    origin_wallet: String,
    destination_chain: String,
    destination_wallet: Option<String>,
    mint_recipient: Option<String>,
    amount: Option<u64>,
    status: String,
    mint_tx_hash: Option<String>,
    error: Option<String>,
    burned_at_ms: Option<i64>,
    attested_at_ms: Option<i64>,
    minted_at_ms: Option<i64>,
    /// End-to-end bridge time (source burn → destination mint); null in flight.
    duration_ms: Option<i64>,
    created_at_ms: i64,
}

impl From<TransferRow> for TransferDto {
    fn from(r: TransferRow) -> Self {
        let burned = r.burned_at.map(|t| t.timestamp_millis());
        let minted = r.minted_at.map(|t| t.timestamp_millis());
        Self {
            duration_ms: match (burned, minted) {
                (Some(b), Some(m)) if r.status == status::COMPLETE => Some(m - b),
                _ => None,
            },
            destination_chain: r.destination_chain().to_string(),
            id: r.id,
            origin_chain: r.origin_chain,
            origin_tx_hash: r.origin_tx_hash,
            origin_wallet: r.origin_wallet,
            destination_wallet: r.destination_wallet,
            mint_recipient: r.mint_recipient,
            amount: r.amount.and_then(|a| a.to_u64()),
            status: r.status,
            mint_tx_hash: r.mint_tx_hash,
            error: r.error,
            burned_at_ms: burned,
            attested_at_ms: r.attested_at.map(|t| t.timestamp_millis()),
            minted_at_ms: minted,
            created_at_ms: r.created_at.timestamp_millis(),
        }
    }
}

async fn create_transfer(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTransferReq>,
) -> Result<Json<TransferDto>, (StatusCode, String)> {
    if req.origin_chain != chain::SUI && req.origin_chain != chain::SOLANA {
        return Err((StatusCode::BAD_REQUEST, "origin_chain must be 'sui' or 'solana'".into()));
    }
    let tx_hash = req.tx_hash.trim().to_string();
    if tx_hash.is_empty() || tx_hash.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "bad tx_hash".into()));
    }
    if req.wallet.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "wallet is required".into()));
    }

    let new = NewTransfer {
        origin_chain: req.origin_chain,
        origin_tx_hash: tx_hash,
        origin_wallet: req.wallet.trim().to_string(),
        destination_wallet: req
            .destination_wallet
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty()),
        status: status::PENDING_ATTESTATION.to_string(),
    };
    let repo = state.repo.clone();
    let row = tokio::task::spawn_blocking(move || repo.insert_transfer(new))
        .await
        .map_err(internal)?
        .map_err(internal)?;
    Ok(Json(row.into()))
}

#[derive(Deserialize)]
struct ListQuery {
    wallet: String,
    #[serde(default)]
    open: bool,
}

async fn list_transfers(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<TransferDto>>, (StatusCode, String)> {
    let repo = state.repo.clone();
    let rows =
        tokio::task::spawn_blocking(move || repo.transfers_for_wallet(&q.wallet, q.open))
            .await
            .map_err(internal)?
            .map_err(internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
