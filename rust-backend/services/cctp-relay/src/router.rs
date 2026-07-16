//! HTTP API.
//!
//! - `GET /health` — liveness.
//! - `GET /config` — the CCTP constants for the bridged networks. This
//!   service owns them: the frontend fetches them here rather than carrying
//!   its own copy keyed on `VITE_ENVIRONMENT`, which is what let staging
//!   (protocol on testnet, bridge on mainnet) pair a mainnet bridge with
//!   testnet Circle ids.
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

use crate::config::Config;
use crate::db::models::{chain, status, NewTransfer, TransferRow};
use crate::solana_mint::{MESSAGE_TRANSMITTER, TOKEN_MESSENGER_MINTER};
use crate::state::AppState;
use crate::{DOMAIN_SOLANA, DOMAIN_SUI};

pub async fn serve(addr: SocketAddr, state: Arc<AppState>, allowed_origins: &[String]) -> Result<()> {
    let cors = build_cors(allowed_origins)?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/config", get(get_config))
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

/// Circle's CCTP v1 deployment on the two bridged networks, as served to the
/// frontend. Every value here is Circle's — the protocol publishes no bridge
/// contract of its own; both burn legs call Circle directly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CctpConfigDto {
    pub domain_sui: u32,
    pub domain_solana: u32,
    pub sui: SuiCctpDto,
    pub solana: SolanaCctpDto,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiCctpDto {
    /// `testnet` | `mainnet` — the network the burn PTB must be signed
    /// against. Independent of the protocol's own network.
    pub network: String,
    pub message_transmitter_package: String,
    pub token_messenger_package: String,
    pub message_transmitter_state: String,
    pub token_messenger_state: String,
    pub usdc_treasury: String,
    pub usdc_coin_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolanaCctpDto {
    pub network: String,
    pub rpc_url: String,
    pub usdc_mint: String,
    pub token_messenger_program: String,
    pub message_transmitter_program: String,
}

impl CctpConfigDto {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            domain_sui: DOMAIN_SUI,
            domain_solana: DOMAIN_SOLANA,
            sui: SuiCctpDto {
                network: cfg.sui.network.to_string(),
                message_transmitter_package: cfg.sui.message_transmitter_package.clone(),
                token_messenger_package: cfg.sui.token_messenger_minter_package.clone(),
                message_transmitter_state: cfg.sui.message_transmitter_state.clone(),
                token_messenger_state: cfg.sui.token_messenger_minter_state.clone(),
                usdc_treasury: cfg.sui.usdc_treasury.clone(),
                usdc_coin_type: cfg.sui.usdc_coin_type.clone(),
            },
            solana: SolanaCctpDto {
                network: cfg.solana.network.clone(),
                rpc_url: cfg.solana.rpc_url.clone(),
                usdc_mint: cfg.solana.usdc_mint.clone(),
                token_messenger_program: TOKEN_MESSENGER_MINTER.to_string(),
                message_transmitter_program: MESSAGE_TRANSMITTER.to_string(),
            },
        }
    }
}

async fn get_config(State(state): State<Arc<AppState>>) -> Json<CctpConfigDto> {
    Json(state.cctp_config.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /config` is the frontend's only source of CCTP constants, so the
    /// wire keys are a contract (frontend/src/api/cctpConfig.ts) and the
    /// similarly-named state ids must not get crossed. Builds the DTO from the
    /// real dev config and pins both.
    #[test]
    fn config_dto_matches_the_dev_config() {
        let cfg = Config::load("config/config.toml").expect("loading dev config");
        let v = serde_json::to_value(CctpConfigDto::from_config(&cfg)).unwrap();

        assert_eq!(v["domainSui"], 8);
        assert_eq!(v["domainSolana"], 5);

        // Each id lands on the field the burn PTB expects, not its neighbour.
        assert_eq!(v["sui"]["tokenMessengerPackage"], cfg.sui.token_messenger_minter_package);
        assert_eq!(v["sui"]["messageTransmitterPackage"], cfg.sui.message_transmitter_package);
        assert_eq!(v["sui"]["tokenMessengerState"], cfg.sui.token_messenger_minter_state);
        assert_eq!(v["sui"]["messageTransmitterState"], cfg.sui.message_transmitter_state);
        assert_eq!(v["sui"]["usdcTreasury"], cfg.sui.usdc_treasury);
        assert_eq!(v["sui"]["usdcCoinType"], cfg.sui.usdc_coin_type);

        assert_eq!(v["solana"]["usdcMint"], cfg.solana.usdc_mint);
        assert_eq!(v["solana"]["rpcUrl"], cfg.solana.rpc_url);
        assert_eq!(v["solana"]["tokenMessengerProgram"], TOKEN_MESSENGER_MINTER);
        assert_eq!(v["solana"]["messageTransmitterProgram"], MESSAGE_TRANSMITTER);
    }
}
