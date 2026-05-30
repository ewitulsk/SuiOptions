//! `GET /call-token-lots?wallet=0x…` — provenance for owned `CallOption`s.
//!
//! Returns the list of `WriteExecuted` events where `call_token_recipient
//! == wallet`. Each entry is one purchase ("lot"). The frontend uses these
//! to populate `boughtFrom`, `premiumPaid`, `boughtAt` on the dashboard's
//! owned-call cards, and to drive an expandable "purchase history" view.
//!
//! ### What's NOT returned here
//!
//! - Current `CallOption` holdings. Those come from `suiClient.getOwnedObjects`
//!   on the frontend — the wallet is the source of truth for what the user
//!   currently holds. A lot is *history*, not balance.
//! - Provenance for `CallOption` objects produced by `call_option::split`.
//!   The split child has a new object id with no corresponding
//!   `WriteExecuted`. The dashboard falls back to "derived from your
//!   holdings in this bucket" for those.

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
pub struct LotsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct LotDto {
    pub call_option_id: String,
    pub bucket_id: String,
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub asset_coin_type: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_coin_type: String,
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    /// `CallOption.amount` at mint, in underlying smallest units.
    pub amount_raw: String,
    /// `WriteExecuted.gross_premium`. Net = gross − fee, but the buyer
    /// pays gross (the fee is skimmed from the seller's side per §3.3.4).
    pub premium_paid_raw: String,
    /// Account that signed the quote. Used by the frontend to render
    /// `boughtFrom` — typically as a short-hex MM id.
    pub seller_account_id: String,
    pub timestamp_ms: i64,
}

#[derive(Serialize)]
pub struct LotsResponse {
    pub lots: Vec<LotDto>,
}

pub async fn list_call_token_lots(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LotsQuery>,
) -> Json<LotsResponse> {
    let wallet = match SuiAddress::from_hex(&q.wallet) {
        Ok(a) => a,
        Err(_) => return Json(LotsResponse { lots: vec![] }),
    };

    let buckets = state.buckets_by_id();
    let mut lots: Vec<LotDto> = state
        .lots_for_recipient(&wallet)
        .into_iter()
        .filter_map(|lot| {
            let bucket = buckets.get(&lot.bucket_id)?;
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
            Some(LotDto {
                call_option_id: lot.call_option_id.to_hex(),
                bucket_id: lot.bucket_id.to_hex(),
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
                amount_raw: lot.amount.to_string(),
                premium_paid_raw: lot.premium_paid.to_string(),
                seller_account_id: lot.seller_account_id.to_hex(),
                timestamp_ms: lot.timestamp_ms as i64,
            })
        })
        .collect();

    // Most recent first — UI expects newest purchases at the top.
    lots.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    Json(LotsResponse { lots })
}
