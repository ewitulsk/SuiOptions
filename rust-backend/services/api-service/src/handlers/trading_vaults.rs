//! Curated trading vault endpoints (SO-287):
//!
//!   - `GET /trading-vaults`     — list with headline state / observed pps
//!   - `GET /trading-vaults/:id` — one vault + its adapter positions (past
//!     positions included, `active=false`)
//!
//! All reads are JIT GraphQL queries to the indexer. Balance-precise NAV
//! needs object reads and isn't served here.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use indexer_graphql::TradingVault;
use protocol_types::ids::ObjectId;

use crate::state::AppState;

/// pps is a 1e12-scaled deposit-asset-per-share.
const PPS_SCALE: f64 = 1e12;

#[derive(Serialize)]
pub struct TradingVaultDto {
    pub vault_id: String,
    pub deposit_symbol: String,
    pub deposit_decimals: Option<u8>,
    pub deposit_coin_type: String,
    pub creator: String,
    /// Current curator wallet (updated on curator rotation).
    pub curator: String,
    pub curator_cap_id: String,
    /// open | closing | closed.
    pub state: String,
    pub lockup_ms: i64,
    pub curator_fee_bps: i64,
    pub rotation_authority: u8,
    pub max_positions: i64,
    pub unwind_grace_ms: i64,
    pub deposits_paused: bool,
    pub mm_release_enabled: bool,
    pub total_shares_raw: String,
    pub position_count: i64,
    pub pending_withdrawals: i64,
    /// Observed deposit-asset-per-share at the last deposit/withdraw
    /// (PPS_SCALE-adjusted).
    pub pps: Option<f64>,
    pub pps_raw: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Serialize)]
pub struct TradingVaultsResponse {
    pub vaults: Vec<TradingVaultDto>,
}

#[derive(Serialize)]
pub struct TradingVaultPositionDto {
    pub position_id: String,
    pub adapter: String,
    pub active: bool,
    pub stored_at_ms: i64,
    pub removed_at_ms: Option<i64>,
}

#[derive(Serialize)]
pub struct TradingVaultDetailResponse {
    #[serde(flatten)]
    pub vault: TradingVaultDto,
    pub positions: Vec<TradingVaultPositionDto>,
}

pub async fn list_trading_vaults(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TradingVaultsResponse>, StatusCode> {
    let vaults = state.indexer.trading_vaults().await.map_err(|e| {
        tracing::warn!(error = %e, "indexer trading_vaults query failed");
        StatusCode::BAD_GATEWAY
    })?;
    Ok(Json(TradingVaultsResponse {
        vaults: vaults.iter().map(|v| trading_vault_dto(&state, v)).collect(),
    }))
}

pub async fn get_trading_vault(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<TradingVaultDetailResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The indexer serves the full list only (a handful of vaults); pick ours.
    let vault = state
        .indexer
        .trading_vaults()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading_vaults query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .find(|v| v.vault_id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let positions = state.indexer.trading_vault_positions(id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer trading_vault_positions query failed");
        StatusCode::BAD_GATEWAY
    })?;
    Ok(Json(TradingVaultDetailResponse {
        vault: trading_vault_dto(&state, &vault),
        positions: positions
            .into_iter()
            .map(|p| TradingVaultPositionDto {
                position_id: p.position_id.to_hex(),
                adapter: p.adapter.to_canonical(),
                active: p.active,
                stored_at_ms: p.stored_at_ms as i64,
                removed_at_ms: p.removed_at_ms.map(|v| v as i64),
            })
            .collect(),
    }))
}

fn trading_vault_dto(state: &AppState, v: &TradingVault) -> TradingVaultDto {
    let meta = state.catalog.lookup(v.deposit_asset.as_str());
    TradingVaultDto {
        vault_id: v.vault_id.to_hex(),
        deposit_symbol: meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| v.deposit_asset.as_str().to_string()),
        deposit_decimals: meta.map(|m| m.decimals),
        deposit_coin_type: v.deposit_asset.to_canonical(),
        creator: v.creator.to_hex(),
        curator: v.curator.to_hex(),
        curator_cap_id: v.curator_cap_id.to_hex(),
        state: v.state.clone(),
        lockup_ms: v.lockup_ms as i64,
        curator_fee_bps: v.curator_fee_bps as i64,
        rotation_authority: v.rotation_authority,
        max_positions: v.max_positions as i64,
        unwind_grace_ms: v.unwind_grace_ms as i64,
        deposits_paused: v.deposits_paused,
        mm_release_enabled: v.mm_release_enabled,
        total_shares_raw: v.total_shares.to_string(),
        position_count: v.position_count as i64,
        pending_withdrawals: v.pending_withdrawals as i64,
        pps: v.latest_pps_e12.map(|p| p as f64 / PPS_SCALE),
        pps_raw: v.latest_pps_e12.map(|p| p.to_string()),
        updated_at_ms: v.updated_at_ms as i64,
    }
}
