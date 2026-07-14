//! `GET /auctions` — the venue's generalized auction list (replaces the
//! Sui twin's `/rfqs`). The mm-bot polls `?status=open` as its discovery
//! fallback; dashboards read the full lifecycle. JIT GraphQL to the
//! indexer, like every read.
//!
//! Modes: `covered_call` and `cash_secured_put` auctions carry a bucket;
//! pure `swap` auctions have `bucket_id: null`. Scaled floats use the
//! token catalog: `amount` scales by the escrow mint's decimals, the bid
//! fields by the bid mint's.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::handlers::buckets::scale_u128;
use crate::ids;
use crate::state::{AppState, Auction};

#[derive(Deserialize)]
pub struct AuctionsQuery {
    /// open | settled | unsold.
    pub status: Option<String>,
    /// swap | covered_call | cash_secured_put.
    pub mode: Option<String>,
    /// Bucket filter (base58) — option auctions for one bucket.
    pub bucket: Option<String>,
    /// Creator filter (base58) — e.g. a vault PDA for its coupled auctions.
    pub creator: Option<String>,
}

#[derive(Serialize)]
pub struct AuctionDto {
    pub auction_id: String,
    /// `swap` | `covered_call` | `cash_secured_put`.
    pub mode: String,
    /// `null` for pure swaps.
    pub bucket_id: Option<String>,
    pub creator: String,
    /// Mint of the escrowed side (base58).
    pub escrow_mint: String,
    pub escrow_symbol: String,
    pub escrow_decimals: Option<u8>,
    /// Mint bids are denominated in (base58).
    pub bid_mint: String,
    pub bid_symbol: String,
    pub bid_decimals: Option<u8>,
    /// Escrowed amount, scaled by the escrow mint's decimals.
    pub amount: Option<f64>,
    pub amount_raw: String,
    /// Notional in bid-mint units.
    pub notional: Option<f64>,
    pub notional_raw: String,
    pub reserve_bid: Option<f64>,
    pub reserve_bid_raw: String,
    pub best_bid: Option<f64>,
    pub best_bid_raw: Option<String>,
    pub best_bidder: Option<String>,
    pub deadline_ms: i64,
    pub max_deadline_ms: i64,
    pub min_increment_bps: i64,
    pub settle_authority: Option<String>,
    /// `open` | `settled` | `unsold`.
    pub status: String,
    pub winner: Option<String>,
    pub token_recipient: Option<String>,
    /// Position account created at settle (option auctions only).
    pub position_id: Option<String>,
    /// Bid before the protocol fee (settled auctions only).
    pub gross_bid_raw: Option<String>,
    /// Protocol fee taken at settle (settled auctions only).
    pub fee_raw: Option<String>,
    pub net_proceeds_raw: Option<String>,
    /// Whether the escrowed best bid was refunded (unsold auctions).
    pub bid_refunded: Option<bool>,
}

#[derive(Serialize)]
pub struct AuctionsResponse {
    pub auctions: Vec<AuctionDto>,
}

pub async fn list_auctions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AuctionsQuery>,
) -> Result<Json<AuctionsResponse>, StatusCode> {
    if let Some(s) = q.status.as_deref() {
        if !matches!(s, "open" | "settled" | "unsold") {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if let Some(m) = q.mode.as_deref() {
        if !matches!(m, "swap" | "covered_call" | "cash_secured_put") {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    for id in [q.bucket.as_deref(), q.creator.as_deref()].into_iter().flatten() {
        if !ids::is_pubkey(id) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let rows = state
        .indexer
        .auctions(
            q.status.as_deref(),
            q.mode.as_deref(),
            q.bucket.as_deref(),
            q.creator.as_deref(),
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer auctions query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let auctions = rows.into_iter().map(|a| auction_dto(&state, a)).collect();
    Ok(Json(AuctionsResponse { auctions }))
}

fn auction_dto(state: &AppState, a: Auction) -> AuctionDto {
    let escrow_meta = state.catalog.lookup(&a.escrow_mint);
    let bid_meta = state.catalog.lookup(&a.bid_mint);
    let escrow_decimals = escrow_meta.map(|m| m.decimals);
    let bid_decimals = bid_meta.map(|m| m.decimals);
    let e_scale = |v: u64| escrow_decimals.map(|d| scale_u128(v as u128, d));
    let b_scale = |v: u64| bid_decimals.map(|d| scale_u128(v as u128, d));
    AuctionDto {
        auction_id: a.auction_id,
        mode: a.mode,
        bucket_id: a.bucket_id,
        creator: a.creator,
        escrow_symbol: escrow_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| a.escrow_mint.clone()),
        escrow_decimals,
        escrow_mint: a.escrow_mint,
        bid_symbol: bid_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| a.bid_mint.clone()),
        bid_decimals,
        bid_mint: a.bid_mint,
        amount: e_scale(a.amount),
        amount_raw: a.amount.to_string(),
        notional: b_scale(a.notional),
        notional_raw: a.notional.to_string(),
        reserve_bid: b_scale(a.reserve_bid),
        reserve_bid_raw: a.reserve_bid.to_string(),
        best_bid: a.best_bid.and_then(b_scale),
        best_bid_raw: a.best_bid.map(|v| v.to_string()),
        best_bidder: a.best_bidder,
        deadline_ms: a.deadline_ms as i64,
        max_deadline_ms: a.max_deadline_ms as i64,
        min_increment_bps: a.min_increment_bps as i64,
        settle_authority: a.settle_authority,
        status: a.status,
        winner: a.winner,
        token_recipient: a.token_recipient,
        position_id: a.position_id,
        gross_bid_raw: a.gross_bid.map(|v| v.to_string()),
        fee_raw: a.fee.map(|v| v.to_string()),
        net_proceeds_raw: a.net_proceeds.map(|v| v.to_string()),
        bid_refunded: a.bid_refunded,
    }
}

/// One bid in an auction's history, ascending by sequence.
#[derive(Serialize)]
pub struct AuctionBidDto {
    pub sequence: i64,
    pub bidder: String,
    pub token_recipient: String,
    pub bid_raw: String,
    pub previous_bid_raw: String,
    /// Auction deadline after this bid (anti-snipe extensions move it).
    pub deadline_ms: i64,
}

#[derive(Serialize)]
pub struct AuctionBidsResponse {
    pub auction_id: String,
    pub bids: Vec<AuctionBidDto>,
}

/// `GET /auctions/:auction_id/bids` — the ascending bid history for one
/// auction. The vault track record joins this per round (creator = vault
/// → its auctions → their bids).
pub async fn list_auction_bids(
    State(state): State<Arc<AppState>>,
    Path(auction_id): Path<String>,
) -> Result<Json<AuctionBidsResponse>, StatusCode> {
    if !ids::is_pubkey(&auction_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let bids = state.indexer.auction_bids(&auction_id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer auction_bids query failed");
        StatusCode::BAD_GATEWAY
    })?;
    let bids = bids
        .into_iter()
        .map(|b| AuctionBidDto {
            sequence: b.sequence as i64,
            bidder: b.bidder,
            token_recipient: b.token_recipient,
            bid_raw: b.bid.to_string(),
            previous_bid_raw: b.previous_bid.to_string(),
            deadline_ms: b.deadline_ms as i64,
        })
        .collect();
    Ok(Json(AuctionBidsResponse { auction_id, bids }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TokenCatalog;
    use solana_token_info_client::SupportedToken;

    const MINT_TBTC: &str = "So11111111111111111111111111111111111111112";
    const MINT_TUSDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn state() -> AppState {
        let tok = |ticker: &str, mint: &str, decimals: u8| SupportedToken {
            mint: mint.into(),
            ticker: ticker.into(),
            name: ticker.into(),
            logo_uri: None,
            decimals,
            pyth_feed_id: None,
            enabled: true,
        };
        AppState::new(
            TokenCatalog::from_tokens(&[
                tok("TBTC", MINT_TBTC, 8),
                tok("TUSDC", MINT_TUSDC, 6),
            ]),
            "http://127.0.0.1:9002/graphql".to_string(),
            None,
            "https://api.devnet.solana.com".to_string(),
            None,
        )
    }

    fn auction() -> Auction {
        Auction {
            auction_id: "Auc111".into(),
            mode: "covered_call".into(),
            bucket_id: Some("Bkt111".into()),
            creator: "Cre111".into(),
            escrow_mint: MINT_TBTC.into(),
            bid_mint: MINT_TUSDC.into(),
            amount: 420_000_000,   // 4.2 TBTC(8)
            notional: 357_000_000_000, // 357,000 TUSDC(6)
            reserve_bid: 50_000_000,   // 50 TUSDC
            deadline_ms: 1_760_000_000_000,
            max_deadline_ms: 1_760_000_600_000,
            min_increment_bps: 25,
            settle_authority: None,
            best_bid: Some(75_000_000),
            best_bidder: Some("Bid111".into()),
            status: "settled".into(),
            winner: Some("Bid111".into()),
            token_recipient: Some("Rcp111".into()),
            position_id: Some("Pos111".into()),
            gross_bid: Some(75_000_000),
            fee: Some(3_000_000),
            net_proceeds: Some(72_000_000),
            bid_refunded: None,
        }
    }

    #[test]
    fn auction_dto_scales_by_the_right_mints() {
        let s = state();
        let dto = auction_dto(&s, auction());
        assert_eq!(dto.escrow_symbol, "TBTC");
        assert_eq!(dto.bid_symbol, "TUSDC");
        // amount scales by escrow (TBTC, 8), bids by bid mint (TUSDC, 6).
        assert!((dto.amount.unwrap() - 4.2).abs() < 1e-9);
        assert!((dto.reserve_bid.unwrap() - 50.0).abs() < 1e-9);
        assert!((dto.best_bid.unwrap() - 75.0).abs() < 1e-9);
        assert_eq!(dto.amount_raw, "420000000");
        assert_eq!(dto.best_bid_raw.as_deref(), Some("75000000"));
        assert_eq!(dto.gross_bid_raw.as_deref(), Some("75000000"));
        assert_eq!(dto.status, "settled");
        assert_eq!(dto.mode, "covered_call");
    }

    #[test]
    fn unknown_mints_null_scaled_fields_but_keep_raw() {
        let s = state();
        let mut a = auction();
        a.escrow_mint = "UnknownMint111".into();
        a.bid_mint = "UnknownMint222".into();
        let dto = auction_dto(&s, a);
        assert_eq!(dto.escrow_symbol, "UnknownMint111"); // raw fallback
        assert_eq!(dto.amount, None);
        assert_eq!(dto.best_bid, None);
        assert_eq!(dto.amount_raw, "420000000");
    }

    #[test]
    fn swap_auction_has_no_bucket() {
        let s = state();
        let mut a = auction();
        a.mode = "swap".into();
        a.bucket_id = None;
        let dto = auction_dto(&s, a);
        assert_eq!(dto.bucket_id, None);
        assert_eq!(dto.mode, "swap");
    }
}
