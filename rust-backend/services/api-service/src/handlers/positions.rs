//! `GET /positions?wallet=0x…` — list of `Position` objects held by a wallet.
//!
//! Each row gives the frontend everything it needs to build a
//! `bucket::redeem_position` PTB and to render the writer-side card:
//! - `position_object_id` for the PTB argument
//! - bucket id + strike + expiry + cursor for the rangebar math
//! - `range_start` / `range_end` for FIFO assignment display
//!
//! ### Caveats
//!
//! The `recipient` filter uses the mint-time `position_recipient` field
//! from `WriteExecuted`. Until the indexer walks Sui checkpoint object
//! transfers, a `Position` that the user transferred P2P will still
//! appear in the original owner's list. Acceptable for MVP because no
//! UI currently exposes a way to transfer `Position` objects, but worth
//! revisiting before mainnet.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use protocol_types::ids::SuiAddress;

use crate::handlers::buckets::strike_raw_to_usd;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct PositionsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct PositionDto {
    pub position_object_id: String,
    pub bucket_id: String,
    /// Friendly symbol from the catalog; raw Move type when unknown.
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    /// Full Move coin type (e.g. `0xtp::tbtc::TBTC`). Used by the
    /// frontend to substitute the `Underlying` type arg in PTBs.
    pub asset_coin_type: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    /// Full Move coin type for the settlement asset.
    pub settlement_coin_type: String,
    /// Strike in USD-equivalent whole units. `null` if decimals unknown.
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    /// Range on the bucket's number line. Sent as raw strings to preserve
    /// u128 precision; the frontend can divide by `asset_decimals` for
    /// display.
    pub range_start_raw: String,
    pub range_end_raw: String,
    /// Bucket-wide totals at read time. Cursor moves whenever someone
    /// exercises; total_written grows as new writes happen.
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    /// Gross premium the writer received at mint, in settlement-asset
    /// smallest units. Frontend divides by `settlement_decimals` for USDC
    /// display.
    pub premium_received_raw: String,
    /// `signer_account_id` from the originating `WriteExecuted`. The
    /// counterparty whose Account supplied/received the premium — the
    /// frontend renders this as a short-hex MM id under `soldTo`.
    pub mm_account_id: String,
    pub minted_at_ms: i64,
}

#[derive(Serialize)]
pub struct PositionsResponse {
    pub positions: Vec<PositionDto>,
}

pub async fn list_positions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PositionsQuery>,
) -> Json<PositionsResponse> {
    let wallet = match SuiAddress::from_hex(&q.wallet) {
        Ok(a) => a,
        Err(_) => return Json(PositionsResponse { positions: vec![] }),
    };

    let buckets = state.buckets_by_id();
    let mut positions: Vec<PositionDto> = state
        .positions_for_recipient(&wallet)
        .into_iter()
        .filter_map(|(object_id, p)| {
            let bucket = buckets.get(&p.bucket_id)?;
            let asset_meta = state.catalog.lookup(bucket.asset_type.as_str());
            let settle_meta = state.catalog.lookup(bucket.settlement_type.as_str());
            let asset_decimals = asset_meta.map(|m| m.decimals);
            let settle_decimals = settle_meta.map(|m| m.decimals);
            let strike = match (asset_decimals, settle_decimals) {
                (Some(u), Some(s)) => Some(strike_raw_to_usd(
                    bucket.strike,
                    bucket.strike_scale,
                    u,
                    s,
                )),
                _ => None,
            };
            Some(PositionDto {
                position_object_id: object_id.to_hex(),
                bucket_id: p.bucket_id.to_hex(),
                asset_symbol: asset_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| bucket.asset_type.as_str().to_string()),
                asset_decimals,
                asset_coin_type: bucket.asset_type.as_str().to_string(),
                settlement_symbol: settle_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| bucket.settlement_type.as_str().to_string()),
                settlement_decimals: settle_decimals,
                settlement_coin_type: bucket.settlement_type.as_str().to_string(),
                strike,
                strike_raw: bucket.strike.to_string(),
                strike_scale: bucket.strike_scale,
                expiry_ms: bucket.expiry_ms as i64,
                range_start_raw: p.range_start.to_string(),
                range_end_raw: p.range_end.to_string(),
                total_written_raw: bucket.total_written.to_string(),
                exercise_cursor_raw: bucket.exercise_cursor.to_string(),
                premium_received_raw: p.premium_received.to_string(),
                mm_account_id: p.mm_account_id.to_hex(),
                minted_at_ms: p.timestamp_ms as i64,
            })
        })
        .collect();

    // Sort by expiry then by range_start for stable UI ordering.
    positions.sort_by(|a, b| {
        a.expiry_ms
            .cmp(&b.expiry_ms)
            .then_with(|| a.range_start_raw.cmp(&b.range_start_raw))
    });

    Json(PositionsResponse { positions })
}
