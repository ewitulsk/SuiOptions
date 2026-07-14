//! `GET /events?wallet=<base58>` — the connected wallet's on-chain
//! activity feed.
//!
//! Sourced just-in-time from the indexer's `events(participant: wallet)`
//! query. The indexer records a per-event address fan-out, so one query
//! returns everything the feed needs. Each indexed event is then
//! specialised to this wallet's perspective: a `WriteExecuted` is a
//! "write" for the position recipient and a "buy" for the option-token
//! recipient. `bucket` is joined to its bucket row for
//! symbol/strike/expiry; the frontend composes the human title/body.
//!
//! Event payloads are the raw Solana event JSON (snake_case field names
//! from `solana-contracts/programs/*/src/events.rs`, base58 pubkeys,
//! decimal-string ints), so the projection reads fields dynamically
//! rather than through a typed `ChainEvent` union.
//!
//! Solana deltas vs the Sui twin: put/collateralized/burn families are
//! included; venue auctions surface as `auction_bid` / `auction_settled`
//! rows; every row carries the transaction `signature` (the Sui feed's
//! `tx_hash` was always null).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handlers::buckets::{iso_millis, strike_raw_to_usd};
use crate::ids;
use crate::state::{AppState, IndexerBucket};

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
    /// position_opened | exercise | claim | burn | deposit | withdraw |
    /// auction_bid | auction_settled.
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
    /// Transaction signature (base58) — explorer-linkable.
    pub signature: String,
}

#[derive(Serialize)]
pub struct EventsResponse {
    pub events: Vec<EventDto>,
}

/// Which token's decimals/symbol scale a row's signed `value`. For
/// write/exercise/claim it's read off the bucket; for deposit/withdraw
/// it's the moved mint carried on the event itself.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ValueAsset {
    BucketSettlement,
    BucketUnderlying,
    Mint(String),
}

/// One wallet-perspective activity row, before catalog/bucket enrichment.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    event_type: &'static str,
    /// writer | trader | account.
    side: &'static str,
    bucket_id: Option<String>,
    underlying_amount: Option<u64>,
    value_amount: Option<u64>,
    /// +1 inflow / −1 outflow.
    value_sign: i8,
    value_asset: ValueAsset,
}

fn pstr<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(|v| v.as_str())
}

fn pu64(payload: &Value, field: &str) -> Option<u64> {
    pstr(payload, field).and_then(|s| s.parse().ok())
}

/// The event's bucket field, with the venue's zero-pubkey "no bucket"
/// sentinel mapped to `None` (pure swaps).
fn pbucket(payload: &Value) -> Option<String> {
    pstr(payload, "bucket")
        .and_then(ids::non_zero)
        .map(str::to_string)
}

/// Project one indexed event into the activity rows that belong to
/// `wallet`. A single write yields two rows when the wallet is both the
/// writer and the buyer. Pure — no catalog/bucket lookups — so it's
/// unit-testable against canned payload JSON.
fn rows_for(event_type: &str, payload: &Value, wallet: &str) -> Vec<Row> {
    let mut out = Vec::new();
    let is = |field: &str| pstr(payload, field) == Some(wallet);
    match event_type {
        "WriteExecuted" | "PutWriteExecuted" => {
            let buyer_field = if event_type == "WriteExecuted" {
                "call_token_recipient"
            } else {
                "put_token_recipient"
            };
            if is("position_recipient") {
                // Writer opened a position, received net premium.
                out.push(Row {
                    event_type: "position_opened",
                    side: "writer",
                    bucket_id: pbucket(payload),
                    underlying_amount: pu64(payload, "write_amount"),
                    value_amount: pu64(payload, "net_premium"),
                    value_sign: 1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
            if is(buyer_field) {
                // Buyer bought the option, paid the gross premium.
                out.push(Row {
                    event_type: "position_opened",
                    side: "trader",
                    bucket_id: pbucket(payload),
                    underlying_amount: pu64(payload, "write_amount"),
                    value_amount: pu64(payload, "gross_premium"),
                    value_sign: -1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
        }
        "CollateralizedWrite" | "PutCollateralizedWrite" if is("writer") => {
            // Self-collateralized write: a position opens but no premium
            // moves. The call event carries `amount`, the put `write_amount`.
            let amount = pu64(payload, "write_amount").or_else(|| pu64(payload, "amount"));
            out.push(Row {
                event_type: "position_opened",
                side: "writer",
                bucket_id: pbucket(payload),
                underlying_amount: amount,
                value_amount: None,
                value_sign: 1,
                value_asset: ValueAsset::BucketSettlement,
            });
        }
        // Call exercise: pay strike, receive underlying — surface the
        // underlying inflow (Sui-twin convention).
        "Exercised" if is("exerciser") => out.push(Row {
            event_type: "exercise",
            side: "trader",
            bucket_id: pbucket(payload),
            underlying_amount: pu64(payload, "amount"),
            value_amount: pu64(payload, "amount"),
            value_sign: 1,
            value_asset: ValueAsset::BucketUnderlying,
        }),
        // Put exercise: deliver underlying, receive settlement — surface
        // the settlement inflow.
        "PutExercised" if is("exerciser") => out.push(Row {
            event_type: "exercise",
            side: "trader",
            bucket_id: pbucket(payload),
            underlying_amount: pu64(payload, "amount"),
            value_amount: pu64(payload, "settlement_paid"),
            value_sign: 1,
            value_asset: ValueAsset::BucketSettlement,
        }),
        "Redeemed" | "PutRedeemed" if is("redeemer") => out.push(Row {
            event_type: "claim",
            side: "writer",
            bucket_id: pbucket(payload),
            underlying_amount: pu64(payload, "underlying_returned"),
            value_amount: pu64(payload, "settlement_returned"),
            value_sign: 1,
            value_asset: ValueAsset::BucketSettlement,
        }),
        "ExpiredOptionBurned" | "PutExpiredOptionBurned" if is("burner") => out.push(Row {
            event_type: "burn",
            side: "trader",
            bucket_id: pbucket(payload),
            underlying_amount: pu64(payload, "amount"),
            value_amount: None,
            value_sign: -1,
            value_asset: ValueAsset::BucketSettlement,
        }),
        // Deposits/withdraws matched this wallet via the participant
        // filter (account owner), so every returned one is attributable.
        "AccountDeposit" => out.push(Row {
            event_type: "deposit",
            side: "account",
            bucket_id: None,
            underlying_amount: None,
            value_amount: pu64(payload, "amount"),
            value_sign: 1,
            value_asset: ValueAsset::Mint(pstr(payload, "mint").unwrap_or_default().to_string()),
        }),
        "AccountWithdraw" => out.push(Row {
            event_type: "withdraw",
            side: "account",
            bucket_id: None,
            underlying_amount: None,
            value_amount: pu64(payload, "amount"),
            value_sign: -1,
            value_asset: ValueAsset::Mint(pstr(payload, "mint").unwrap_or_default().to_string()),
        }),
        // Venue: a bid escrows the bid amount. Premium bids on option
        // auctions are settlement-denominated; pure swaps carry no bucket
        // so the value stays unscaled (null) there.
        "AuctionBid" if is("bidder") => out.push(Row {
            event_type: "auction_bid",
            side: "trader",
            bucket_id: None, // AuctionBid carries no bucket field
            underlying_amount: None,
            value_amount: None, // bid mint unknown from this payload alone
            value_sign: -1,
            value_asset: ValueAsset::BucketSettlement,
        }),
        "AuctionSettled" => {
            if is("winner") {
                out.push(Row {
                    event_type: "auction_settled",
                    side: "trader",
                    bucket_id: pbucket(payload),
                    underlying_amount: pu64(payload, "amount"),
                    value_amount: pbucket(payload).and(pu64(payload, "gross_bid")),
                    value_sign: -1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
            if is("position_recipient") {
                out.push(Row {
                    event_type: "auction_settled",
                    side: "writer",
                    bucket_id: pbucket(payload),
                    underlying_amount: pu64(payload, "amount"),
                    value_amount: pbucket(payload).and(pu64(payload, "net_proceeds")),
                    value_sign: 1,
                    value_asset: ValueAsset::BucketSettlement,
                });
            }
        }
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
    if !ids::is_pubkey(&q.wallet) {
        return Ok(Json(EventsResponse { events: vec![] }));
    }

    let events = state
        .indexer
        .events_for_participant(&q.wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    // Buckets are fetched once and joined locally for symbol/strike/expiry.
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

    // Newest first. `events_for_participant` returns ascending by
    // sequence, so iterate in reverse.
    let mut out: Vec<EventDto> = Vec::new();
    for ev in events.iter().rev() {
        for row in rows_for(&ev.event_type, &ev.payload, &q.wallet) {
            out.push(enrich(
                &state,
                ev.sequence,
                ev.timestamp_ms,
                &ev.signature,
                &buckets,
                row,
            ));
        }
    }

    Ok(Json(EventsResponse { events: out }))
}

/// Enrich a wallet-perspective [`Row`] into an `EventDto`: join the bucket
/// for symbol/strike/expiry, scale the underlying by the bucket's asset
/// decimals, and scale the signed value by its `value_asset`'s decimals.
fn enrich(
    state: &AppState,
    sequence: u64,
    ts_ms: u64,
    signature: &str,
    buckets: &BTreeMap<String, IndexerBucket>,
    row: Row,
) -> EventDto {
    let bucket = row.bucket_id.as_ref().and_then(|id| buckets.get(id));
    let asset_meta = bucket.and_then(|b| state.catalog.lookup(&b.underlying_mint));
    let settle_meta = bucket.and_then(|b| state.catalog.lookup(&b.settlement_mint));
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

    // Resolve which token scales the value, then look up decimals/symbol.
    let value_meta = match &row.value_asset {
        ValueAsset::BucketSettlement => settle_meta,
        ValueAsset::BucketUnderlying => asset_meta,
        ValueAsset::Mint(m) => state.catalog.lookup(m),
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
        bucket_id: bucket.map(|b| b.bucket_id.clone()),
        asset_symbol: asset_meta.map(|m| m.symbol.clone()),
        settlement_symbol: settle_meta.map(|m| m.symbol.clone()),
        strike,
        expiry_ms: bucket.map(|b| b.expiry_ms as i64),
        amount,
        value_delta,
        value_unit,
        signature: signature.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WRITER: &str = "Wr1111111111111111111111111111111111111111";
    const BUYER: &str = "By1111111111111111111111111111111111111111";
    const BUCKET: &str = "Bk1111111111111111111111111111111111111111";

    /// Canned WriteExecuted payload exactly as the indexer serves it:
    /// snake_case fields, base58 pubkeys, decimal-string ints.
    fn write_executed() -> Value {
        json!({
            "bucket": BUCKET,
            "signer_account": "Acc111",
            "signer_token_recipient": BUYER,
            "executor": WRITER,
            "position": "Pos111",
            "position_recipient": WRITER,
            "call_token_recipient": BUYER,
            "write_amount": "100",
            "gross_premium": "90",
            "fee": "5",
            "net_premium": "85",
            "range_start": "0",
            "range_end": "100",
            "nonce": "1",
        })
    }

    #[test]
    fn write_executed_splits_into_writer_and_buyer_rows() {
        let payload = write_executed();

        // Writer's perspective: one "writer" row, net premium in.
        let w = rows_for("WriteExecuted", &payload, WRITER);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].side, "writer");
        assert_eq!(w[0].event_type, "position_opened");
        assert_eq!(w[0].value_amount, Some(85));
        assert_eq!(w[0].value_sign, 1);
        assert_eq!(w[0].bucket_id.as_deref(), Some(BUCKET));

        // Buyer's perspective: one "trader" row, gross premium out.
        let b = rows_for("WriteExecuted", &payload, BUYER);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].side, "trader");
        assert_eq!(b[0].value_amount, Some(90));
        assert_eq!(b[0].value_sign, -1);

        // An unrelated wallet sees nothing.
        assert!(rows_for("WriteExecuted", &payload, "Other111").is_empty());
    }

    #[test]
    fn put_write_uses_put_token_recipient() {
        let payload = json!({
            "bucket": BUCKET,
            "position_recipient": WRITER,
            "put_token_recipient": BUYER,
            "write_amount": "50",
            "collateral": "5000",
            "gross_premium": "9",
            "net_premium": "8",
        });
        let b = rows_for("PutWriteExecuted", &payload, BUYER);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].side, "trader");
        assert_eq!(b[0].value_amount, Some(9));
    }

    #[test]
    fn deposit_is_an_account_inflow_in_the_moved_mint() {
        let payload = json!({
            "account": "Acc111",
            "mint": "Mint111",
            "amount": "1000",
        });
        // The participant query already scoped this to the owner, so it
        // always produces a row regardless of the wallet passed.
        let rows = rows_for("AccountDeposit", &payload, WRITER);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "deposit");
        assert_eq!(rows[0].side, "account");
        assert_eq!(rows[0].value_sign, 1);
        assert_eq!(rows[0].value_asset, ValueAsset::Mint("Mint111".into()));
        assert_eq!(rows[0].bucket_id, None);
    }

    #[test]
    fn exercised_and_burn_rows() {
        let ex = json!({ "bucket": BUCKET, "exerciser": BUYER, "amount": "40",
                          "settlement_paid": "3400", "cursor_after": "40" });
        let rows = rows_for("Exercised", &ex, BUYER);
        assert_eq!(rows[0].event_type, "exercise");
        assert_eq!(rows[0].value_asset, ValueAsset::BucketUnderlying);
        assert_eq!(rows[0].value_amount, Some(40));

        // Put exercise surfaces the settlement inflow instead.
        let pex = rows_for("PutExercised", &ex, BUYER);
        assert_eq!(pex[0].value_asset, ValueAsset::BucketSettlement);
        assert_eq!(pex[0].value_amount, Some(3400));

        let burn = json!({ "bucket": BUCKET, "burner": BUYER, "amount": "7" });
        let rows = rows_for("ExpiredOptionBurned", &burn, BUYER);
        assert_eq!(rows[0].event_type, "burn");
        assert_eq!(rows[0].underlying_amount, Some(7));
        assert_eq!(rows[0].value_amount, None);
    }

    #[test]
    fn auction_settled_rows_per_role() {
        let payload = json!({
            "auction": "Auc111",
            "mode": "covered_call",
            "bucket": BUCKET,
            "winner": BUYER,
            "token_recipient": BUYER,
            "position": "Pos111",
            "position_recipient": WRITER,
            "amount": "100",
            "notional": "8500",
            "gross_bid": "90",
            "fee": "5",
            "net_proceeds": "85",
        });
        let w = rows_for("AuctionSettled", &payload, BUYER);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].event_type, "auction_settled");
        assert_eq!(w[0].side, "trader");
        assert_eq!(w[0].value_amount, Some(90));
        assert_eq!(w[0].value_sign, -1);

        let c = rows_for("AuctionSettled", &payload, WRITER);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].side, "writer");
        assert_eq!(c[0].value_amount, Some(85));
        assert_eq!(c[0].value_sign, 1);
    }

    #[test]
    fn swap_auction_zero_bucket_maps_to_none() {
        // Pure swaps carry Pubkey::default() as "no bucket".
        let payload = json!({
            "auction": "Auc111",
            "mode": "swap",
            "bucket": ids::ZERO_PUBKEY,
            "winner": BUYER,
            "token_recipient": BUYER,
            "position": ids::ZERO_PUBKEY,
            "position_recipient": ids::ZERO_PUBKEY,
            "amount": "10",
            "notional": "0",
            "gross_bid": "9",
            "fee": "0",
            "net_proceeds": "9",
        });
        let rows = rows_for("AuctionSettled", &payload, BUYER);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bucket_id, None);
        // No bucket → no settlement decimals to scale by → value withheld.
        assert_eq!(rows[0].value_amount, None);
    }

    #[test]
    fn auction_bid_row_for_bidder_only() {
        let payload = json!({
            "auction": "Auc111",
            "bidder": BUYER,
            "token_recipient": BUYER,
            "bid": "75",
            "previous_bid": "70",
            "deadline_ms": "1760000000000",
        });
        let rows = rows_for("AuctionBid", &payload, BUYER);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "auction_bid");
        assert!(rows_for("AuctionBid", &payload, WRITER).is_empty());
    }
}
