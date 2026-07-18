//! Curated trading vault endpoints (SO-287):
//!
//!   - `GET /trading-vaults`     — list with headline state / observed pps
//!   - `GET /trading-vaults/:id` — one vault + its adapter positions (past
//!     positions included, `active=false`)
//!
//! Event-derived analytics (SO-293):
//!
//!   - `GET /trading-vaults/:id/pps-history`     — observed pps points from
//!     TvDeposited / TvWithdrawFulfilled events, ascending by time
//!   - `GET /trading-vaults/:id/stake/:address`  — one wallet's live stake
//!     replayed from TvDeposited / TvWithdrawRequested events
//!
//! All reads are JIT GraphQL queries to the indexer. Balance-precise NAV
//! needs object reads and isn't served here.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::json;

use indexer_graphql::TradingVault;
use protocol_types::events::ChainEvent;
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::state::AppState;

/// pps is a 1e12-scaled deposit-asset-per-share.
const PPS_SCALE: f64 = 1e12;

/// pps scale as an integer, for exact event-derived arithmetic.
const PPS_E12: u128 = 1_000_000_000_000;

/// Cap on the per-vault event scans backing the analytics endpoints. The
/// indexer serves the most recent events first, so a vault with more matching
/// events than this silently loses its OLDEST history (earliest pps points /
/// earliest stake flows), not the newest.
const EVENT_SCAN_CAP: usize = 5000;

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

// ── event-derived analytics (SO-293) ──────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PpsPointDto {
    /// Event time (ms since epoch), decimal string.
    pub timestamp_ms: String,
    /// 1e12-scaled deposit-asset-per-share, decimal string.
    pub pps_e12: String,
    /// `deposit` | `fulfillment`.
    pub source: String,
}

#[derive(Serialize)]
pub struct PpsHistoryResponse {
    pub points: Vec<PpsPointDto>,
}

/// `GET /trading-vaults/:id/pps-history` — observed pps points, ascending by
/// time. Each TvDeposited implies pps = amount/shares; each
/// TvWithdrawFulfilled implies pps = value/shares (zero-share / zero-value
/// events carry no price and are skipped).
pub async fn get_pps_history(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<PpsHistoryResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvDeposited", "TvWithdrawFulfilled"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;

    let mut points = Vec::new();
    for ev in &events {
        let (pps_e12, source) = match &ev.event {
            ChainEvent::TvDeposited(d) if d.shares != 0 => {
                (d.amount as u128 * PPS_E12 / d.shares, "deposit")
            }
            ChainEvent::TvWithdrawFulfilled(f) if f.shares != 0 && f.value != 0 => {
                (f.value as u128 * PPS_E12 / f.shares, "fulfillment")
            }
            _ => continue,
        };
        points.push(PpsPointDto {
            timestamp_ms: ev.timestamp_ms.to_string(),
            pps_e12: pps_e12.to_string(),
            source: source.to_string(),
        });
    }
    Ok(Json(PpsHistoryResponse { points }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StakeResponse {
    /// Live share balance, decimal string (u128).
    pub shares: String,
    /// Deposit-asset cost basis of the live shares, decimal string (u64).
    pub cost_basis: String,
    /// shares × latest observed pps / 1e12; null when the vault has no
    /// observed pps yet.
    pub estimated_value: Option<String>,
    /// Lockup expiry from the wallet's most recent deposit; null if the
    /// wallet never deposited.
    pub locked_until_ms: Option<String>,
}

/// `GET /trading-vaults/:id/stake/:address` — one wallet's live stake,
/// replayed from the vault's deposit / withdraw-request events. Curator
/// cap-keyed stakes (`curator_cap != null`) are out of scope — address
/// stakes only.
pub async fn get_stake(
    State(state): State<Arc<AppState>>,
    Path((vault_id, address)): Path<(String, String)>,
) -> Result<Json<StakeResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let addr = SuiAddress::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
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
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvDeposited", "TvWithdrawRequested"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;

    let mut shares: u128 = 0;
    let mut cost_basis: u64 = 0;
    let mut locked_until_ms: Option<u64> = None;
    for ev in &events {
        match &ev.event {
            ChainEvent::TvDeposited(d) if d.depositor == addr && d.curator_cap.is_none() => {
                shares = shares.saturating_add(d.shares);
                cost_basis = cost_basis.saturating_add(d.amount);
                locked_until_ms = Some(d.locked_until_ms);
            }
            ChainEvent::TvWithdrawRequested(w)
                if w.recipient == addr && w.curator_cap.is_none() =>
            {
                shares = shares.saturating_sub(w.shares);
                cost_basis = cost_basis.saturating_sub(w.basis);
            }
            _ => {}
        }
    }
    // shares × pps can't realistically overflow u128 (shares ≲ 1e20,
    // pps ≲ 1e15), but degrade to null rather than a wrong number if it does.
    let estimated_value = vault
        .latest_pps_e12
        .and_then(|pps| shares.checked_mul(pps))
        .map(|v| (v / PPS_E12).to_string());
    Ok(Json(StakeResponse {
        shares: shares.to_string(),
        cost_basis: cost_basis.to_string(),
        estimated_value,
        locked_until_ms: locked_until_ms.map(|v| v.to_string()),
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
