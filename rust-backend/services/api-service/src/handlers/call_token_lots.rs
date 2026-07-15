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

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use protocol_types::events::ChainEvent;
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::handlers::buckets::strike_raw_to_usd;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LotsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct LotDto {
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
) -> Result<Json<LotsResponse>, StatusCode> {
    let wallet = match SuiAddress::from_hex(&q.wallet) {
        Ok(a) => a,
        Err(_) => return Ok(Json(LotsResponse { lots: vec![] })),
    };

    // A "lot" is one WriteExecuted whose call_token_recipient is this wallet.
    // We reconstruct them straight from the event log, then join each to its
    // bucket (fetched once into a map) for strike/expiry/symbols.
    let events = state
        .indexer
        .write_executed_for_recipient(wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer write-executed query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let buckets: BTreeMap<ObjectId, indexer_graphql::Bucket> = state
        .indexer
        .buckets(false, None, None, None)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer buckets query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .map(|b| (b.bucket_id, b))
        .collect();

    let mut lots: Vec<LotDto> = events
        .into_iter()
        .filter_map(|ev| {
            let ChainEvent::WriteExecuted(w) = ev.event else {
                return None;
            };
            let bucket = buckets.get(&w.bucket_id)?;
            let asset_meta = state.catalog.lookup(bucket.asset_type.as_str());
            let settle_meta = state.catalog.lookup(bucket.settlement_type.as_str());
            let asset_decimals = asset_meta.map(|m| m.decimals);
            let settle_decimals = settle_meta.map(|m| m.decimals);
            let strike = match (asset_decimals, settle_decimals) {
                (Some(u), Some(s)) => {
                    Some(strike_raw_to_usd(bucket.strike, bucket.strike_scale, u, s))
                }
                _ => None,
            };
            Some(LotDto {
                bucket_id: w.bucket_id.to_hex(),
                asset_symbol: asset_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| bucket.asset_type.as_str().to_string()),
                asset_decimals,
                asset_coin_type: bucket.asset_type.to_canonical(),
                settlement_symbol: settle_meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| bucket.settlement_type.as_str().to_string()),
                settlement_decimals: settle_decimals,
                settlement_coin_type: bucket.settlement_type.to_canonical(),
                strike,
                strike_raw: bucket.strike.to_string(),
                strike_scale: bucket.strike_scale,
                expiry_ms: bucket.expiry_ms as i64,
                amount_raw: w.write_amount.to_string(),
                premium_paid_raw: w.gross_premium.to_string(),
                seller_account_id: w.signer_id.to_hex(),
                timestamp_ms: ev.timestamp_ms as i64,
            })
        })
        .collect();

    // Most recent first — UI expects newest purchases at the top.
    lots.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    Ok(Json(LotsResponse { lots }))
}
