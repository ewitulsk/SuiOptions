//! `GET /call-token-lots?wallet=<base58>` — provenance for owned option
//! tokens.
//!
//! Returns the `WriteExecuted` events where `call_token_recipient ==
//! wallet` plus the `PutWriteExecuted` events where `put_token_recipient
//! == wallet`. Each entry is one purchase ("lot"). The frontend uses these
//! to populate `boughtFrom`, `premiumPaid`, `boughtAt` on the dashboard's
//! owned-option cards.
//!
//! ### What's NOT returned here
//!
//! Current option-token balances. Those come from the wallet's SPL token
//! accounts on the frontend — the wallet is the source of truth for what
//! the user currently holds. A lot is *history*, not balance.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::handlers::buckets::strike_raw_to_usd;
use crate::ids;
use crate::state::{AppState, IndexedEvent, IndexerBucket};

#[derive(Deserialize)]
pub struct LotsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct LotDto {
    pub bucket_id: String,
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
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    /// Write amount at mint, in underlying smallest units.
    pub amount_raw: String,
    /// `gross_premium` — the buyer pays gross (the fee is skimmed from
    /// the seller's side).
    pub premium_paid_raw: String,
    /// MM Account that signed the quote. The frontend renders this as a
    /// short id under `boughtFrom`.
    pub seller_account_id: String,
    /// Purchase transaction signature (base58), for explorer links.
    pub signature: String,
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
    if !ids::is_pubkey(&q.wallet) {
        return Ok(Json(LotsResponse { lots: vec![] }));
    }

    // A "lot" is one WriteExecuted / PutWriteExecuted whose option-token
    // recipient is this wallet. Reconstructed straight from the event log,
    // then joined to buckets (fetched once into a map) for strike/expiry/
    // symbols.
    let mut events = state
        .indexer
        .write_executed_for_recipient(&q.wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer write-executed query failed");
            StatusCode::BAD_GATEWAY
        })?;
    events.extend(
        state
            .indexer
            .put_write_executed_for_recipient(&q.wallet)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "indexer put-write-executed query failed");
                StatusCode::BAD_GATEWAY
            })?,
    );
    let buckets: BTreeMap<String, IndexerBucket> = state
        .indexer
        .buckets(false, None, None, None, None, None)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer buckets query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .map(|b| (b.bucket_id.clone(), b))
        .collect();

    let mut lots: Vec<LotDto> = events
        .iter()
        .filter_map(|ev| lot_from_event(&state, ev, &buckets))
        .collect();

    // Most recent first — UI expects newest purchases at the top.
    lots.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));

    Ok(Json(LotsResponse { lots }))
}

/// Project one write event's raw payload into a lot, joined to its bucket.
/// `None` when the payload is malformed or the bucket is unknown.
fn lot_from_event(
    state: &AppState,
    ev: &IndexedEvent,
    buckets: &BTreeMap<String, IndexerBucket>,
) -> Option<LotDto> {
    let bucket_id = ev.payload_str("bucket").ok()?;
    let bucket = buckets.get(bucket_id)?;
    let asset_meta = state.catalog.lookup(&bucket.underlying_mint);
    let settle_meta = state.catalog.lookup(&bucket.settlement_mint);
    let asset_decimals = asset_meta.map(|m| m.decimals);
    let settle_decimals = settle_meta.map(|m| m.decimals);
    let strike = match (asset_decimals, settle_decimals) {
        (Some(u), Some(s)) => Some(strike_raw_to_usd(bucket.strike, bucket.strike_scale, u, s)),
        _ => None,
    };
    Some(LotDto {
        bucket_id: bucket_id.to_string(),
        asset_symbol: asset_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| bucket.underlying_mint.clone()),
        asset_decimals,
        asset_mint: bucket.underlying_mint.clone(),
        settlement_symbol: settle_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| bucket.settlement_mint.clone()),
        settlement_decimals: settle_decimals,
        settlement_mint: bucket.settlement_mint.clone(),
        option_mint: bucket.option_mint.clone(),
        option_kind: bucket.option_kind.clone(),
        strike,
        strike_raw: bucket.strike.to_string(),
        strike_scale: bucket.strike_scale,
        expiry_ms: bucket.expiry_ms as i64,
        amount_raw: ev.payload_u64("write_amount").ok()?.to_string(),
        premium_paid_raw: ev.payload_u64("gross_premium").ok()?.to_string(),
        seller_account_id: ev.payload_str("signer_account").ok()?.to_string(),
        signature: ev.signature.clone(),
        timestamp_ms: ev.timestamp_ms as i64,
    })
}
