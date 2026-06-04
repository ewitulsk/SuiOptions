//! Catalog handlers.
//!
//! Public (read): [`list_tokens`], [`get_token`], [`package_info`].
//! Internal (mutate): [`create_token`], [`update_token`], [`delete_token`].

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::info;

use deployments::PackageInfo;
use token_info_client::SupportedToken;

use crate::db::models::UpsertToken;
use crate::state::AppState;

type ApiError = (StatusCode, String);

fn internal_err(e: anyhow::Error) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ---------------------------------------------------------------- public

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `?enabled=true` returns only enabled tokens.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `GET /tokens` — the supported-token catalog.
pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SupportedToken>>, ApiError> {
    let rows = state
        .repo
        .list(q.enabled.unwrap_or(false))
        .map_err(internal_err)?;
    Ok(Json(rows.into_iter().map(|r| r.into_dto()).collect()))
}

/// `GET /tokens/:coin_type` — a single token, or 404.
pub async fn get_token(
    State(state): State<Arc<AppState>>,
    Path(coin_type): Path<String>,
) -> Result<Json<SupportedToken>, ApiError> {
    match state.repo.get(&coin_type).map_err(internal_err)? {
        Some(row) => Ok(Json(row.into_dto())),
        None => Err((StatusCode::NOT_FOUND, format!("no token {coin_type}"))),
    }
}

/// `GET /package-info` — protocol on-chain ids for the configured network
/// (+ testTokens passthrough). Read once from `deployments.json` at boot.
pub async fn package_info(State(state): State<Arc<AppState>>) -> Json<PackageInfo> {
    Json(state.package_info.clone())
}

// -------------------------------------------------------------- internal

#[derive(Debug, Deserialize)]
pub struct UpsertTokenReq {
    pub coin_type: String,
    pub ticker: String,
    pub name: String,
    #[serde(default)]
    pub logo_uri: Option<String>,
    pub decimals: u8,
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl UpsertTokenReq {
    fn into_row(self, source: &str) -> UpsertToken {
        UpsertToken {
            coin_type: self.coin_type,
            ticker: self.ticker,
            name: self.name,
            logo_uri: self.logo_uri,
            decimals: self.decimals as i16,
            pyth_feed_id: self.pyth_feed_id,
            enabled: self.enabled,
            source: source.to_string(),
        }
    }
}

/// `POST /tokens` — add or replace a supported token (internal).
pub async fn create_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertTokenReq>,
) -> Result<Json<SupportedToken>, ApiError> {
    let coin_type = req.coin_type.clone();
    let row = state
        .repo
        .upsert(req.into_row("manual"))
        .map_err(internal_err)?;
    info!(%coin_type, "token upserted via internal API");
    Ok(Json(row.into_dto()))
}

/// `PUT /tokens/:coin_type` — update a token (internal). The path coin type
/// wins over any value in the body.
pub async fn update_token(
    State(state): State<Arc<AppState>>,
    Path(coin_type): Path<String>,
    Json(mut req): Json<UpsertTokenReq>,
) -> Result<Json<SupportedToken>, ApiError> {
    req.coin_type = coin_type.clone();
    let row = state
        .repo
        .upsert(req.into_row("manual"))
        .map_err(internal_err)?;
    info!(%coin_type, "token updated via internal API");
    Ok(Json(row.into_dto()))
}

/// `DELETE /tokens/:coin_type` — remove a token (internal). 404 if absent.
pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    Path(coin_type): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state.repo.delete(&coin_type).map_err(internal_err)?;
    if removed == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no token {coin_type}")));
    }
    info!(%coin_type, "token deleted via internal API");
    Ok(StatusCode::NO_CONTENT)
}
