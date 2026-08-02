//! Supported-asset catalog and rates.
//!
//! Dakota exposes neither. `/capabilities/networks` returns bare network id
//! strings with no asset information, and `GET /self-serve/credits/pricing`
//! 403s for our client tier ("Credit management is only available for
//! self-serve customers"). So the catalog is ours to curate, and the fee
//! schedule is admin-entered.
//!
//! Realised rates are a different matter: every transaction receipt carries an
//! `exchange_rate` and a fee breakdown, so `GET /rates` reports what we were
//! actually charged alongside what we expected to be.

use std::sync::Arc;

use auth_client::VerifiedClaims;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Serialize;

use super::{internal, ApiError};
use crate::authz::Caller;
use crate::db::models::{Asset, FeeSchedule, NewFeeSchedule, UpsertAsset};
use crate::state::AppState;

#[derive(Serialize)]
pub struct CatalogResp {
    pub assets: Vec<Asset>,
    /// Networks Dakota accepts, as reported by `/capabilities/networks`,
    /// intersected with what our config permits. Sandbox lists mainnets it
    /// then refuses, so the intersection is the honest answer.
    pub networks: Vec<String>,
}

/// `GET /catalog` — everything the ramp forms need to render. Readable by any
/// authenticated caller; it describes our offering, not anyone's data.
pub async fn get_catalog(State(state): State<Arc<AppState>>) -> Result<Json<CatalogResp>, ApiError> {
    let assets = state.repo.list_assets().map_err(internal)?;

    // Best-effort: a Dakota outage should degrade the dropdown, not break the
    // page. Falling back to the configured allow-list keeps it usable.
    let networks = match state
        .dakota
        .get::<Vec<String>>("GET /capabilities/networks", "/capabilities/networks")
        .await
    {
        Ok(all) => all
            .into_iter()
            .filter(|n| state.cfg.network_allowed(n))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "falling back to the configured network list");
            state.cfg.dakota.allowed_networks.clone()
        }
    };

    Ok(Json(CatalogResp { assets, networks }))
}

/// `PUT /admin/assets` — add or update one `(symbol, network)` entry.
pub async fn upsert_asset(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(req): Json<UpsertAsset>,
) -> Result<Json<Asset>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;

    if !state.cfg.network_allowed(&req.network_id) {
        return Err(super::bad_request(format!(
            "network {} is not permitted in this environment",
            req.network_id
        )));
    }
    state.repo.upsert_asset(&req).map(Json).map_err(internal)
}

/// `DELETE /admin/assets/:id`
pub async fn delete_asset(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Path(id): Path<i32>,
) -> Result<axum::http::StatusCode, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    let removed = state.repo.delete_asset(id).map_err(internal)?;
    Ok(if removed == 0 {
        axum::http::StatusCode::NOT_FOUND
    } else {
        axum::http::StatusCode::NO_CONTENT
    })
}

#[derive(Serialize)]
pub struct RatesResp {
    /// What we expect to be charged. `source` is `manual` unless Dakota ever
    /// opens the pricing endpoint to us — surfaced so the UI can say so rather
    /// than implying Dakota confirmed these numbers.
    pub schedule: Option<FeeSchedule>,
    /// What we were actually charged, newest first, derived from receipts.
    pub realised: Vec<RealisedRate>,
}

#[derive(Serialize)]
pub struct RealisedRate {
    pub asset: Option<String>,
    pub exchange_rate: Option<String>,
    pub fee_minor: Option<i64>,
    pub amount_minor: Option<i64>,
    pub occurred_at: Option<String>,
}

/// `GET /rates`
pub async fn get_rates(State(state): State<Arc<AppState>>) -> Result<Json<RatesResp>, ApiError> {
    let schedule = state.repo.current_fees().map_err(internal)?;
    let realised = state
        .repo
        .list_events(None, 50)
        .map_err(internal)?
        .into_iter()
        .filter(|e| e.exchange_rate.is_some())
        .map(|e| RealisedRate {
            asset: e.asset,
            exchange_rate: e.exchange_rate,
            fee_minor: e.fee_minor,
            amount_minor: e.amount_minor,
            occurred_at: e.occurred_at.map(|t| t.to_rfc3339()),
        })
        .collect();
    Ok(Json(RatesResp { schedule, realised }))
}

/// `POST /admin/rates` — record the expected fee schedule.
pub async fn set_rates(
    State(state): State<Arc<AppState>>,
    Extension(claims): Extension<VerifiedClaims>,
    Json(req): Json<NewFeeSchedule>,
) -> Result<Json<FeeSchedule>, ApiError> {
    Caller::from_claims(&claims)?.require_admin()?;
    state.repo.record_fees(&req).map(Json).map_err(internal)
}
