//! Catalog handlers.
//!
//! Public (read): [`list_tokens`], [`get_token`], [`program_info`]. On
//! non-mainnet-beta networks the read handlers merge the durable DB catalog
//! with the test-token overlay (DB wins on mint collision).
//!
//! Mutate: [`create_token`], [`update_token`], [`delete_token`] — operate on
//! the DB catalog only; they never touch the overlay. Mints are base58 and
//! compared byte-exact — there is NO normalization; the only validation is
//! that a mint base58-decodes to 32 bytes (400 otherwise).

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::info;

use solana_deployments::ProgramInfo;
use solana_token_info_client::SupportedToken;

use crate::db::models::UpsertToken;
use crate::state::AppState;

type ApiError = (StatusCode, String);

fn internal_err(e: anyhow::Error) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Reject a mint that doesn't base58-decode to exactly 32 bytes. The write
/// endpoints' only validation — reads stay byte-exact lookups.
fn validate_mint(mint: &str) -> Result<(), ApiError> {
    solana_deployments::validate_pubkey(mint, "mint")
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

/// Merge the durable DB catalog with the test-token overlay. `db_tokens`
/// must contain ALL rows (disabled included) so the suppression set covers
/// them — a DB row owns its mint regardless of `enabled`, otherwise a
/// disabled manual override would let the overlay twin reappear. Mints are
/// compared byte-exact (base58 needs no normalization).
fn merge_with_overlay(
    db_tokens: Vec<SupportedToken>,
    overlay: &[SupportedToken],
    enabled_only: bool,
) -> Vec<SupportedToken> {
    let have: HashSet<&str> = db_tokens.iter().map(|t| t.mint.as_str()).collect();

    let mut tokens: Vec<SupportedToken> = overlay
        .iter()
        .filter(|o| !have.contains(o.mint.as_str()))
        .filter(|o| !enabled_only || o.enabled)
        .cloned()
        .collect();
    tokens.extend(
        db_tokens
            .iter()
            .filter(|t| !enabled_only || t.enabled)
            .cloned(),
    );

    tokens.sort_by(|a, b| a.ticker.cmp(&b.ticker));
    tokens
}

// ---------------------------------------------------------------- public

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `?enabled=true` returns only enabled tokens.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `GET /tokens` — durable catalog ∪ test-token overlay.
pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<SupportedToken>>, ApiError> {
    metrics::counter!("solana_token_info_requests_total", "op" => "list").increment(1);
    let enabled_only = q.enabled.unwrap_or(false);

    // Fetch ALL DB rows so the suppression set covers disabled rows too.
    let db_tokens: Vec<SupportedToken> = state
        .repo
        .list(false)
        .map_err(internal_err)?
        .into_iter()
        .map(|r| r.into_dto())
        .collect();

    Ok(Json(merge_with_overlay(
        db_tokens,
        &state.overlay,
        enabled_only,
    )))
}

/// `GET /tokens/:mint` — DB row, else overlay entry, else 404. Byte-exact
/// mint comparison.
pub async fn get_token(
    State(state): State<Arc<AppState>>,
    Path(mint): Path<String>,
) -> Result<Json<SupportedToken>, ApiError> {
    if let Some(row) = state.repo.get(&mint).map_err(internal_err)? {
        return Ok(Json(row.into_dto()));
    }
    if let Some(o) = state.overlay.iter().find(|t| t.mint == mint) {
        return Ok(Json(o.clone()));
    }
    Err((StatusCode::NOT_FOUND, format!("no token {mint}")))
}

/// `GET /program-info` — protocol on-chain ids for the configured env
/// (+ testTokens passthrough). Read once from `solana-deployments.json` at
/// boot and served verbatim.
pub async fn program_info(State(state): State<Arc<AppState>>) -> Json<ProgramInfo> {
    Json(state.program_info.clone())
}

// -------------------------------------------------------------- mutate

#[derive(Debug, Deserialize)]
pub struct UpsertTokenReq {
    pub mint: String,
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
            mint: self.mint,
            ticker: self.ticker,
            name: self.name,
            logo_uri: self.logo_uri,
            decimals: self.decimals as i16,
            pyth_feed_id: self.pyth_feed_id,
            enabled: self.enabled,
        }
    }
}

/// `POST /tokens` — add or replace a supported token in the DB.
pub async fn create_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UpsertTokenReq>,
) -> Result<Json<SupportedToken>, ApiError> {
    metrics::counter!("solana_token_info_requests_total", "op" => "create").increment(1);
    validate_mint(&req.mint)?;
    let mint = req.mint.clone();
    let row = state.repo.upsert(req.into_row()).map_err(internal_err)?;
    info!(%mint, "token upserted");
    Ok(Json(row.into_dto()))
}

/// `PUT /tokens/:mint` — update a token. The path mint wins over any value
/// in the body.
pub async fn update_token(
    State(state): State<Arc<AppState>>,
    Path(mint): Path<String>,
    Json(mut req): Json<UpsertTokenReq>,
) -> Result<Json<SupportedToken>, ApiError> {
    metrics::counter!("solana_token_info_requests_total", "op" => "update").increment(1);
    validate_mint(&mint)?;
    req.mint = mint.clone();
    let row = state.repo.upsert(req.into_row()).map_err(internal_err)?;
    info!(%mint, "token updated");
    Ok(Json(row.into_dto()))
}

/// `DELETE /tokens/:mint` — remove a token from the DB. 404 if absent.
/// (Overlay test tokens are derived, not stored, so they 404 here.)
pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    Path(mint): Path<String>,
) -> Result<StatusCode, ApiError> {
    metrics::counter!("solana_token_info_requests_total", "op" => "delete").increment(1);
    validate_mint(&mint)?;
    let removed = state.repo.delete(&mint).map_err(internal_err)?;
    if removed == 0 {
        return Err((StatusCode::NOT_FOUND, format!("no token {mint}")));
    }
    info!(%mint, "token deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::db::models::TokenRow;

    const MINT_TBTC: &str = "So11111111111111111111111111111111111111112";
    const MINT_TUSDC: &str = "11111111111111111111111111111111";
    const MINT_OTHER: &str = "6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t";

    fn tok(ticker: &str, mint: &str, enabled: bool) -> SupportedToken {
        SupportedToken {
            mint: mint.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals: 8,
            pyth_feed_id: None,
            enabled,
        }
    }

    #[test]
    fn mint_validation_is_bs58_32_bytes() {
        assert!(validate_mint(MINT_TBTC).is_ok());
        assert!(validate_mint(MINT_TUSDC).is_ok());
        // Not base58 (0, l, I, O and 0x-prefix are invalid).
        assert!(validate_mint("0xdeadbeef").is_err());
        // Valid base58 but not 32 bytes.
        assert!(validate_mint("abc").is_err());
        assert!(validate_mint("").is_err());
        // 400, not 500.
        assert_eq!(validate_mint("abc").unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn merge_db_wins_on_mint_collision() {
        // DB row and overlay twin share a mint: only the DB row survives.
        let db = vec![tok("TBTC-DB", MINT_TBTC, true)];
        let overlay = vec![tok("TBTC", MINT_TBTC, true), tok("TUSDC", MINT_TUSDC, true)];
        let merged = merge_with_overlay(db, &overlay, false);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|t| t.ticker == "TBTC-DB"));
        assert!(merged.iter().any(|t| t.ticker == "TUSDC"));
        assert!(!merged.iter().any(|t| t.ticker == "TBTC"));
    }

    #[test]
    fn merge_disabled_db_row_still_suppresses_overlay() {
        // A disabled DB row owns its mint: with ?enabled=true neither the row
        // nor its overlay twin appears.
        let db = vec![tok("TBTC", MINT_TBTC, false)];
        let overlay = vec![tok("TBTC", MINT_TBTC, true), tok("TUSDC", MINT_TUSDC, true)];
        let merged = merge_with_overlay(db, &overlay, true);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].ticker, "TUSDC");
    }

    #[test]
    fn merge_is_byte_exact_and_ticker_sorted() {
        // Distinct mints never collide; output is ticker-sorted.
        let db = vec![tok("ZZZ", MINT_OTHER, true)];
        let overlay = vec![tok("TUSDC", MINT_TUSDC, true), tok("TBTC", MINT_TBTC, true)];
        let merged = merge_with_overlay(db, &overlay, false);
        let tickers: Vec<&str> = merged.iter().map(|t| t.ticker.as_str()).collect();
        assert_eq!(tickers, vec!["TBTC", "TUSDC", "ZZZ"]);
    }

    #[test]
    fn dto_mapping_round_trips() {
        // UpsertTokenReq -> UpsertToken (u8 -> i16), TokenRow -> SupportedToken.
        let req: UpsertTokenReq = serde_json::from_str(
            r#"{ "mint": "So11111111111111111111111111111111111111112",
                 "ticker": "TBTC", "name": "Test Bitcoin", "decimals": 8 }"#,
        )
        .unwrap();
        assert!(req.enabled); // defaults true
        let row = req.into_row();
        assert_eq!(row.mint, MINT_TBTC);
        assert_eq!(row.decimals, 8i16);
        assert!(row.logo_uri.is_none());

        let now = Utc::now();
        let dto = TokenRow {
            mint: MINT_TBTC.into(),
            ticker: "TBTC".into(),
            name: "Test Bitcoin".into(),
            logo_uri: Some("https://logo".into()),
            decimals: 8,
            pyth_feed_id: None,
            enabled: false,
            created_at: now,
            updated_at: now,
        }
        .into_dto();
        assert_eq!(dto.mint, MINT_TBTC);
        assert_eq!(dto.decimals, 8u8);
        assert_eq!(dto.logo_uri.as_deref(), Some("https://logo"));
        assert!(!dto.enabled);
    }
}
