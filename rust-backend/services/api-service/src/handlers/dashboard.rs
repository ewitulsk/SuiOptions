//! `POST /dashboard/positions` — enrich a wallet's written positions (SO-97).
//!
//! The frontend reads the authoritative list of `Position` object ids
//! straight from the wallet (`getOwnedObjects`), then posts them here. We
//! query the indexer's GraphQL API for the chain truth (bucket strike/expiry/
//! cursor + the provenance denormalized onto each position) and layer on the
//! catalog (symbol/decimals, USD strike). Ids the indexer doesn't know yet
//! are simply absent from the response — the frontend renders those degraded
//! rather than dropping them, so a write never silently disappears.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::handlers::buckets::strike_raw_to_usd;
use crate::state::{AppState, Position};

#[derive(Deserialize)]
pub struct EnrichRequest {
    pub object_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct EnrichedPositionDto {
    pub position_object_id: String,
    pub bucket_id: String,
    pub asset_symbol: String,
    pub asset_decimals: Option<u8>,
    pub asset_coin_type: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_coin_type: String,
    /// Strike in USD-equivalent whole units; `null` if decimals unknown.
    pub strike: Option<f64>,
    pub strike_raw: String,
    pub strike_scale: u8,
    pub expiry_ms: i64,
    pub range_start_raw: String,
    pub range_end_raw: String,
    pub total_written_raw: String,
    pub exercise_cursor_raw: String,
    pub premium_received_raw: String,
    /// `signer_account_id` from the minting `WriteExecuted` (the MM).
    pub mm_account_id: String,
    /// Minting tx digest, for explorer links. Empty string if unknown.
    pub tx_digest: String,
    pub minted_at_ms: i64,
}

#[derive(Serialize)]
pub struct EnrichResponse {
    pub positions: Vec<EnrichedPositionDto>,
}

pub async fn enrich_positions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EnrichRequest>,
) -> Json<EnrichResponse> {
    let rows = match state.indexer.positions_by_object_ids(&req.object_ids).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "indexer position enrichment failed");
            return Json(EnrichResponse { positions: vec![] });
        }
    };

    let positions = rows
        .into_iter()
        .map(|p| state.enrich_one(p))
        .collect::<Vec<_>>();
    Json(EnrichResponse { positions })
}

impl AppState {
    /// Layer catalog symbols/decimals + USD strike onto an enriched indexer
    /// position.
    fn enrich_one(&self, p: Position) -> EnrichedPositionDto {
        let asset_meta = self.catalog.lookup(p.asset_type.as_str());
        let settle_meta = self.catalog.lookup(p.settlement_type.as_str());
        let asset_decimals = asset_meta.map(|m| m.decimals);
        let settle_decimals = settle_meta.map(|m| m.decimals);

        let strike = match (asset_decimals, settle_decimals) {
            (Some(u), Some(s)) => Some(strike_raw_to_usd(p.strike, p.strike_scale, u, s)),
            _ => None,
        };

        EnrichedPositionDto {
            position_object_id: p.object_id.to_hex(),
            bucket_id: p.bucket_id.to_hex(),
            asset_symbol: asset_meta
                .map(|m| m.symbol.clone())
                .unwrap_or_else(|| p.asset_type.as_str().to_string()),
            asset_decimals,
            asset_coin_type: p.asset_type.to_canonical(),
            settlement_symbol: settle_meta
                .map(|m| m.symbol.clone())
                .unwrap_or_else(|| p.settlement_type.as_str().to_string()),
            settlement_decimals: settle_decimals,
            settlement_coin_type: p.settlement_type.to_canonical(),
            strike,
            strike_raw: p.strike.to_string(),
            strike_scale: p.strike_scale,
            expiry_ms: p.expiry_ms as i64,
            range_start_raw: p.range_start.to_string(),
            range_end_raw: p.range_end.to_string(),
            total_written_raw: p.total_written.to_string(),
            exercise_cursor_raw: p.exercise_cursor.to_string(),
            premium_received_raw: p.premium_received.to_string(),
            mm_account_id: p.mm_account_id.to_hex(),
            tx_digest: p.tx_digest,
            minted_at_ms: p.minted_at_ms as i64,
        }
    }
}
