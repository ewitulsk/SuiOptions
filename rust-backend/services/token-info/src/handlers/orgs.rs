//! Verified-orgs allowlist handlers.
//!
//! Public (read): [`list_orgs`], [`get_org`]. Internal/JWT-gated (mutate):
//! [`create_org`], [`update_org`], [`delete_org`]. Mirrors the token-catalog
//! handlers — this is the platform-controlled allowlist that gates which
//! orgs' buckets the user-facing surfaces (api-service, quoting, frontend)
//! serve. The on-chain org registry itself is permissionless; this table
//! only curates visibility.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::info;

use token_info_client::VerifiedOrg;

use crate::db::models::UpsertOrg;
use crate::state::AppState;

type ApiError = (StatusCode, String);

fn internal_err(e: anyhow::Error) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ---------------------------------------------------------------- public

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `?enabled=true` returns only enabled (i.e. actually verified) orgs.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `GET /orgs` — the verified-orgs allowlist.
pub async fn list_orgs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<VerifiedOrg>>, ApiError> {
    let rows = state
        .repo
        .list_orgs(q.enabled.unwrap_or(false))
        .map_err(internal_err)?;
    Ok(Json(rows.into_iter().map(|r| r.into_dto()).collect()))
}

/// `GET /orgs/:org_id` — one allowlist row, or 404.
pub async fn get_org(
    State(state): State<Arc<AppState>>,
    Path(org_id): Path<String>,
) -> Result<Json<VerifiedOrg>, ApiError> {
    match state.repo.get_org(&org_id).map_err(internal_err)? {
        Some(row) => Ok(Json(row.into_dto())),
        None => Err((StatusCode::NOT_FOUND, format!("no verified org {org_id}"))),
    }
}

// -------------------------------------------------------------- internal

#[derive(Debug, Deserialize)]
pub struct UpsertOrgReq {
    pub org_id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl UpsertOrgReq {
    fn into_row(self) -> UpsertOrg {
        UpsertOrg {
            org_id: self.org_id,
            name: self.name,
            enabled: self.enabled,
        }
    }
}

/// `POST /orgs` — verify (add or replace) an org.
pub async fn create_org(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertOrgReq>,
) -> Result<Json<VerifiedOrg>, ApiError> {
    let org_id = req.org_id.clone();
    let row = state.repo.upsert_org(req.into_row()).map_err(internal_err)?;
    info!(%org_id, "verified org upserted");
    Ok(Json(row.into_dto()))
}

/// `PUT /orgs/:org_id` — update an allowlist row (e.g. `enabled=false` to
/// de-verify while preserving history). The path org id wins over the body.
pub async fn update_org(
    State(state): State<Arc<AppState>>,
    Path(org_id): Path<String>,
    Json(mut req): Json<UpsertOrgReq>,
) -> Result<Json<VerifiedOrg>, ApiError> {
    req.org_id = org_id.clone();
    let row = state.repo.upsert_org(req.into_row()).map_err(internal_err)?;
    info!(%org_id, "verified org updated");
    Ok(Json(row.into_dto()))
}

/// `DELETE /orgs/:org_id` — remove an org from the allowlist entirely. 404 if
/// absent. Prefer `PUT enabled=false` to keep history.
pub async fn delete_org(
    State(state): State<Arc<AppState>>,
    Path(org_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state.repo.delete_org(&org_id).map_err(internal_err)?;
    if removed == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no verified org {org_id}")));
    }
    info!(%org_id, "verified org deleted");
    Ok(StatusCode::NO_CONTENT)
}
