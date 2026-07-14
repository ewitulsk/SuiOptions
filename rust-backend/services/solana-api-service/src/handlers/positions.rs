//! `GET /positions?wallet=<base58>` — writer `Position` accounts held by a
//! wallet.
//!
//! Each row gives the frontend everything it needs to build a redeem
//! transaction and render the writer-side card: `position_id` (the
//! Position account's pubkey — positions are fresh keypairs on Solana),
//! bucket id + strike + expiry + cursor for the rangebar math, and
//! `range_start`/`range_end` for FIFO assignment display.
//!
//! ### Caveats
//!
//! The `recipient` filter uses the mint-time `position_recipient` from
//! `WriteExecuted`/`PutWriteExecuted` (plus `PositionTransferred` where
//! the indexer applies it). A position transferred by a path the indexer
//! doesn't track would still appear under the original owner.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::handlers::buckets::strike_raw_to_usd;
use crate::ids;
use crate::state::{AppState, Position};

#[derive(Deserialize)]
pub struct PositionsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct PositionDto {
    /// Position account pubkey (base58) — the redeem ix argument.
    pub position_id: String,
    pub bucket_id: String,
    /// Friendly symbol from the catalog; raw base58 mint when unknown.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub asset_mint: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_mint: String,
    /// SPL mint of the bucket's option token.
    pub option_mint: String,
    /// `"call"` | `"put"`.
    pub option_kind: String,
    /// Strike in USD-equivalent whole units. `null` if decimals unknown.
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    /// Range on the bucket's number line. Raw strings preserve u128
    /// precision; the frontend can divide by `asset_decimals` for display.
    pub range_start_raw: String,
    pub range_end_raw: String,
    /// Bucket-wide totals at read time.
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    /// Gross premium the writer received at mint, in settlement-asset
    /// smallest units. `"0"` for collateralized (non-quote) writes.
    pub premium_received_raw: String,
    /// `signer_account` from the originating write. `null` for
    /// collateralized writes (no MM counterparty).
    pub mm_account_id: Option<String>,
    /// Minting transaction signature (base58), for explorer links.
    pub signature: String,
    pub minted_at_ms: i64,
}

#[derive(Serialize)]
pub struct PositionsResponse {
    pub positions: Vec<PositionDto>,
}

pub async fn list_positions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PositionsQuery>,
) -> Result<Json<PositionsResponse>, StatusCode> {
    if !ids::is_pubkey(&q.wallet) {
        return Ok(Json(PositionsResponse { positions: vec![] }));
    }

    // The indexer's `positionsByRecipient` returns each position already
    // joined to its bucket, so there's no second lookup to do here.
    let rows = state
        .indexer
        .positions_by_recipient(&q.wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer positions query failed");
            StatusCode::BAD_GATEWAY
        })?;

    let mut positions: Vec<PositionDto> =
        rows.into_iter().map(|p| position_dto(&state, p)).collect();

    // Sort by expiry then by range_start for stable UI ordering.
    positions.sort_by(|a, b| {
        a.expiry_ms
            .cmp(&b.expiry_ms)
            .then_with(|| a.range_start_raw.cmp(&b.range_start_raw))
    });

    Ok(Json(PositionsResponse { positions }))
}

/// Layer catalog symbols/decimals + USD strike onto an enriched indexer
/// position. Shared with `POST /dashboard/positions`.
pub(crate) fn position_dto(state: &AppState, p: Position) -> PositionDto {
    let asset_meta = state.catalog.lookup(&p.underlying_mint);
    let settle_meta = state.catalog.lookup(&p.settlement_mint);
    let asset_decimals = asset_meta.map(|m| m.decimals);
    let settle_decimals = settle_meta.map(|m| m.decimals);
    let strike = match (asset_decimals, settle_decimals) {
        (Some(u), Some(s)) => Some(strike_raw_to_usd(p.strike, p.strike_scale, u, s)),
        _ => None,
    };
    PositionDto {
        position_id: p.position_id,
        bucket_id: p.bucket_id,
        asset_symbol: asset_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| p.underlying_mint.clone()),
        asset_decimals,
        asset_mint: p.underlying_mint,
        settlement_symbol: settle_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| p.settlement_mint.clone()),
        settlement_decimals: settle_decimals,
        settlement_mint: p.settlement_mint,
        option_mint: p.option_mint,
        option_kind: p.option_kind,
        strike,
        strike_raw: p.strike.to_string(),
        strike_scale: p.strike_scale,
        expiry_ms: p.expiry_ms as i64,
        range_start_raw: p.range_start.to_string(),
        range_end_raw: p.range_end.to_string(),
        total_written_raw: p.total_written.to_string(),
        exercise_cursor_raw: p.exercise_cursor.to_string(),
        premium_received_raw: p.premium_received.to_string(),
        mm_account_id: p.mm_account_id,
        signature: p.signature,
        minted_at_ms: p.minted_at_ms as i64,
    }
}
