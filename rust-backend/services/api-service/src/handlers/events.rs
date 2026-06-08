//! `GET /events?wallet=0x…` — the connected wallet's on-chain activity feed.
//!
//! Sourced just-in-time from the indexer's `events(participant: wallet)` query.
//! The indexer records a per-event address fan-out (the recipients of a write,
//! the exerciser, the redeemer, and the account *owner* for deposits and
//! withdraws), so that one query returns everything the feed needs. Each
//! indexer event is then specialised to this wallet's perspective: a
//! `WriteExecuted` is a "write" for the position recipient and a "buy" for the
//! call-token recipient. `bucket_id` is joined to its bucket for
//! symbol/strike/expiry; the frontend composes the human title/body.
//!
//! ### What's not surfaced yet
//! - `tx_hash` is always `null` — the decoded event payload doesn't carry the
//!   digest.
//! - Per-writer "cursor advanced against you" rows (needs range-overlap math).
//! - Off-chain quote lifecycle (signed/expired) — not in the indexer.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use protocol_types::asset::AssetType;
use protocol_types::events::ChainEvent;
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::handlers::buckets::{iso_millis, strike_raw_to_usd};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct EventsQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct EventDto {
    /// Stable id for React keys / dedupe.
    pub id: String,
    pub ts_ms: i64,
    /// Pre-formatted ISO-8601 UTC — use directly as the UI's `ts`.
    pub ts_iso: String,
    /// `EVENT_TYPE_META` key: position_opened | exercise | claim | deposit | withdraw.
    #[serde(rename = "type")]
    pub event_type: String,
    /// writer | trader | account.
    pub side: String,
    /// All indexer events are on-chain confirmed.
    pub status: String,
    pub bucket_id: Option<String>,
    pub asset_symbol: Option<String>,
    pub settlement_symbol: Option<String>,
    /// Strike in USD-equivalent whole units; `null` if decimals unknown.
    pub strike: Option<f64>,
    pub expiry_ms: Option<i64>,
    /// Scaled underlying size (write/buy/exercise amount).
    pub amount: Option<f64>,
    /// Signed value for the UI's `{delta, unit}` (+ inflow / − outflow).
    pub value_delta: Option<f64>,
    pub value_unit: Option<String>,
    /// Always `null` for now — the decoded event doesn't carry the tx digest.
    pub tx_hash: Option<String>,
}

#[derive(Serialize)]
pub struct EventsResponse {
    pub events: Vec<EventDto>,
}

/// Which coin's decimals/symbol scale a row's signed `value`. For
/// write/exercise/claim it's read off the bucket; for deposit/withdraw it's
/// the moved coin carried on the event itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ValueAsset {
    BucketSettlement,
    BucketUnderlying,
    Coin(AssetType),
}

/// One wallet-perspective activity row, before catalog/bucket enrichment.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// `EVENT_TYPE_META` key.
    event_type: &'static str,
    /// writer | trader | account.
    side: &'static str,
    bucket_id: Option<ObjectId>,
    underlying_amount: Option<u64>,
    value_amount: Option<u64>,
    /// +1 inflow / −1 outflow.
    value_sign: i8,
    value_asset: ValueAsset,
}

/// Project one indexer event into the activity rows that belong to `wallet`.
/// A single `WriteExecuted` yields two rows when the wallet is both the writer
/// and the buyer. Pure — no catalog/bucket lookups — so it's unit-testable.
fn rows_for(event: &ChainEvent, wallet: &SuiAddress) -> Vec<Row> {
    let mut out = Vec::new();
    match event {
        ChainEvent::WriteExecuted(w) => {
            if w.position_recipient == *wallet {
                // Writer opened a position, received net premium.
                out.push(Row {
                    event_type: "position_opened",
                    side: "writer",
                    bucket_id: Some(w.bucket_id),
                    underlying_amount: Some(w.write_amount),
                    value_amount: Some(w.net_premium),
                    value_sign: 1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
            if w.call_token_recipient == *wallet {
                // Buyer bought the call, paid the gross premium.
                out.push(Row {
                    event_type: "position_opened",
                    side: "trader",
                    bucket_id: Some(w.bucket_id),
                    underlying_amount: Some(w.write_amount),
                    value_amount: Some(w.gross_premium),
                    value_sign: -1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
        }
        ChainEvent::Exercised(e) if e.exerciser == *wallet => out.push(Row {
            event_type: "exercise",
            side: "trader",
            bucket_id: Some(e.bucket_id),
            underlying_amount: Some(e.amount),
            value_amount: Some(e.amount),
            value_sign: 1,
            value_asset: ValueAsset::BucketUnderlying,
        }),
        ChainEvent::Redeemed(r) if r.redeemer == *wallet => out.push(Row {
            event_type: "claim",
            side: "writer",
            bucket_id: Some(r.bucket_id),
            underlying_amount: Some(r.underlying_returned),
            value_amount: Some(r.settlement_returned),
            value_sign: 1,
            value_asset: ValueAsset::BucketSettlement,
        }),
        // Deposits/withdraws matched this wallet via the account_owner
        // participant role, so every returned one is attributable to it.
        ChainEvent::AccountDeposit(d) => out.push(Row {
            event_type: "deposit",
            side: "account",
            bucket_id: None,
            underlying_amount: None,
            value_amount: Some(d.amount),
            value_sign: 1,
            value_asset: ValueAsset::Coin(d.asset_type.clone()),
        }),
        ChainEvent::AccountWithdraw(w) => out.push(Row {
            event_type: "withdraw",
            side: "account",
            bucket_id: None,
            underlying_amount: None,
            value_amount: Some(w.amount),
            value_sign: -1,
            value_asset: ValueAsset::Coin(w.asset_type.clone()),
        }),
        _ => {}
    }
    out
}

fn scale(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

pub async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, StatusCode> {
    let wallet = match SuiAddress::from_hex(&q.wallet) {
        Ok(a) => a,
        Err(_) => return Ok(Json(EventsResponse { events: vec![] })),
    };

    let events = state
        .indexer
        .events_for_participant(wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    // Buckets are fetched once and joined locally for symbol/strike/expiry.
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

    // Newest first. `events_for_participant` returns ascending by sequence, so
    // iterate in reverse.
    let mut out: Vec<EventDto> = Vec::new();
    for ev in events.iter().rev() {
        for row in rows_for(&ev.event, &wallet) {
            out.push(enrich(&state, ev.sequence, ev.timestamp_ms, &buckets, row));
        }
    }

    Ok(Json(EventsResponse { events: out }))
}

/// Enrich a wallet-perspective [`Row`] into an `EventDto`: join the bucket for
/// symbol/strike/expiry, scale the underlying by the bucket's asset decimals,
/// and scale the signed value by its `value_asset`'s decimals.
fn enrich(
    state: &AppState,
    sequence: u64,
    ts_ms: u64,
    buckets: &BTreeMap<ObjectId, indexer_graphql::Bucket>,
    row: Row,
) -> EventDto {
    let bucket = row.bucket_id.and_then(|id| buckets.get(&id));
    let asset_meta = bucket.and_then(|b| state.catalog.lookup(b.asset_type.as_str()));
    let settle_meta = bucket.and_then(|b| state.catalog.lookup(b.settlement_type.as_str()));
    let asset_decimals = asset_meta.map(|m| m.decimals);

    let strike = match (bucket, asset_meta, settle_meta) {
        (Some(b), Some(a), Some(s)) => {
            Some(strike_raw_to_usd(b.strike, b.strike_scale, a.decimals, s.decimals))
        }
        _ => None,
    };
    let amount = match (row.underlying_amount, asset_decimals) {
        (Some(a), Some(d)) => Some(scale(a, d)),
        _ => None,
    };

    // Resolve which coin scales the value, then look up its decimals/symbol.
    let value_meta = match &row.value_asset {
        ValueAsset::BucketSettlement => settle_meta,
        ValueAsset::BucketUnderlying => asset_meta,
        ValueAsset::Coin(t) => state.catalog.lookup(t.as_str()),
    };
    let (value_delta, value_unit) = match (row.value_amount, value_meta) {
        (Some(v), Some(m)) => (
            Some(row.value_sign as f64 * scale(v, m.decimals)),
            Some(m.symbol.clone()),
        ),
        _ => (None, None),
    };

    EventDto {
        id: format!("evt-{sequence}-{}", row.side),
        ts_ms: ts_ms as i64,
        ts_iso: iso_millis(ts_ms as i64),
        event_type: row.event_type.to_string(),
        side: row.side.to_string(),
        status: "confirmed".to_string(),
        bucket_id: bucket.map(|b| b.bucket_id.to_hex()),
        asset_symbol: asset_meta.map(|m| m.symbol.clone()),
        settlement_symbol: settle_meta.map(|m| m.symbol.clone()),
        strike,
        expiry_ms: bucket.map(|b| b.expiry_ms as i64),
        amount,
        value_delta,
        value_unit,
        tx_hash: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::events::{AccountDeposit, WriteExecuted};

    #[test]
    fn write_executed_splits_into_writer_and_buyer_rows() {
        let bucket = ObjectId::new([0xc1; 32]);
        let writer = SuiAddress::new([0x11; 32]);
        let buyer = SuiAddress::new([0x22; 32]);
        let we = ChainEvent::WriteExecuted(WriteExecuted {
            bucket_id: bucket,
            signer_account_id: ObjectId::new([0x33; 32]),
            signer_token_recipient: buyer,
            executor: writer,
            position_id: ObjectId::new([0x44; 32]),
            position_recipient: writer,
            call_token_recipient: buyer,
            write_amount: 100,
            gross_premium: 90,
            fee: 5,
            net_premium: 85,
            range_start: 0,
            range_end: 100,
            nonce: 1,
        });

        // Writer's perspective: one "writer" row, net premium in.
        let w = rows_for(&we, &writer);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].side, "writer");
        assert_eq!(w[0].event_type, "position_opened");
        assert_eq!(w[0].value_amount, Some(85));
        assert_eq!(w[0].value_sign, 1);

        // Buyer's perspective: one "trader" row, gross premium out.
        let b = rows_for(&we, &buyer);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].side, "trader");
        assert_eq!(b[0].value_amount, Some(90));
        assert_eq!(b[0].value_sign, -1);

        // An unrelated wallet sees nothing.
        assert!(rows_for(&we, &SuiAddress::new([0x99; 32])).is_empty());
    }

    #[test]
    fn deposit_is_an_account_inflow_in_the_moved_coin() {
        let owner = SuiAddress::new([0x11; 32]);
        let ev = ChainEvent::AccountDeposit(AccountDeposit {
            account_id: ObjectId::new([0x33; 32]),
            asset_type: AssetType::new("USDC"),
            amount: 1_000,
        });
        // The participant query already scoped this to `owner`, so it always
        // produces a row regardless of the wallet passed.
        let rows = rows_for(&ev, &owner);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "deposit");
        assert_eq!(rows[0].side, "account");
        assert_eq!(rows[0].value_sign, 1);
        assert_eq!(rows[0].value_asset, ValueAsset::Coin(AssetType::new("USDC")));
        assert_eq!(rows[0].bucket_id, None);
    }
}
