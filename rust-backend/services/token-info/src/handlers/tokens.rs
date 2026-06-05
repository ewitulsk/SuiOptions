//! Catalog handlers.
//!
//! Public (read): [`list_tokens`], [`get_token`], [`package_info`]. On
//! dev/staging the read handlers merge the durable DB catalog with the
//! test-token overlay (DB wins on coin-type collision).
//!
//! Internal (mutate): [`create_token`], [`update_token`], [`delete_token`] —
//! operate on the DB catalog only; they never touch the overlay.

use std::collections::HashSet;
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

/// Normalize a Move type so address-format differences (`0x`-prefix, padding,
/// case) don't cause a DB row and its overlay twin to both appear.
fn normalize_coin_type(s: &str) -> String {
    match s.split_once("::") {
        Some((addr, rest)) => {
            let stripped = addr.strip_prefix("0x").unwrap_or(addr).to_ascii_lowercase();
            format!("{:0>64}::{}", stripped, rest)
        }
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------- public

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `?enabled=true` returns only enabled tokens.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `GET /tokens` — durable catalog ∪ dev/staging test-token overlay.
pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SupportedToken>>, ApiError> {
    let enabled_only = q.enabled.unwrap_or(false);

    // Fetch ALL DB rows so the suppression set covers disabled rows too — a DB
    // row owns its coin type regardless of `enabled`, otherwise a disabled
    // manual override would let the overlay twin reappear.
    let db_rows = state.repo.list(false).map_err(internal_err)?;
    let have: HashSet<String> = db_rows
        .iter()
        .map(|r| normalize_coin_type(&r.coin_type))
        .collect();

    let mut tokens: Vec<SupportedToken> = db_rows
        .into_iter()
        .map(|r| r.into_dto())
        .filter(|t| !enabled_only || t.enabled)
        .collect();

    // Overlay any test token whose coin type the DB doesn't define.
    for o in &state.overlay {
        if enabled_only && !o.enabled {
            continue;
        }
        if !have.contains(&normalize_coin_type(&o.coin_type)) {
            tokens.push(o.clone());
        }
    }

    tokens.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    Ok(Json(tokens))
}

/// `GET /tokens/:coin_type` — DB row, else overlay entry, else 404.
pub async fn get_token(
    State(state): State<Arc<AppState>>,
    Path(coin_type): Path<String>,
) -> Result<Json<SupportedToken>, ApiError> {
    if let Some(row) = state.repo.get(&coin_type).map_err(internal_err)? {
        return Ok(Json(row.into_dto()));
    }
    let want = normalize_coin_type(&coin_type);
    if let Some(o) = state
        .overlay
        .iter()
        .find(|t| normalize_coin_type(&t.coin_type) == want)
    {
        return Ok(Json(o.clone()));
    }
    Err((StatusCode::NOT_FOUND, format!("no token {coin_type}")))
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
    fn into_row(self) -> UpsertToken {
        UpsertToken {
            coin_type: self.coin_type,
            ticker: self.ticker,
            name: self.name,
            logo_uri: self.logo_uri,
            decimals: self.decimals as i16,
            pyth_feed_id: self.pyth_feed_id,
            enabled: self.enabled,
        }
    }
}

/// `POST /tokens` — add or replace a supported token in the DB (internal).
pub async fn create_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertTokenReq>,
) -> Result<Json<SupportedToken>, ApiError> {
    let coin_type = req.coin_type.clone();
    let row = state.repo.upsert(req.into_row()).map_err(internal_err)?;
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
    let row = state.repo.upsert(req.into_row()).map_err(internal_err)?;
    info!(%coin_type, "token updated via internal API");
    Ok(Json(row.into_dto()))
}

/// `DELETE /tokens/:coin_type` — remove a token from the DB (internal). 404 if
/// absent. (Overlay test tokens are derived, not stored, so they 404 here.)
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
