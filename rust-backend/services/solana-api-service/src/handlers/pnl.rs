//! `GET /dashboard/pnl?wallet=<base58>` — true options PnL.
//!
//! Per-(wallet, bucket) **FIFO cost-lot ledger** over the event log.
//! Acquisitions push lots, disposals consume them oldest-first and accrue
//! realized PnL:
//!
//! - Acquire: `WriteExecuted` / `PutWriteExecuted` where the wallet is the
//!   option-token recipient (cost = gross premium), and `AuctionSettled`
//!   option auctions won by the wallet (cost = gross bid).
//! - Dispose: `Exercised` / `PutExercised` (proceeds marked at the option
//!   price at exercise time via solana-price-charting when configured,
//!   falling back to the bucket **strike** when unset or without data —
//!   which, with no DEX ingestion, is always for now), and
//!   `ExpiredOptionBurned` / `PutExpiredOptionBurned` (proceeds = 0, a
//!   realized loss).
//!
//! Everything is computed on demand in **display units** (underlying
//! tokens / settlement USD) so the frontend renders it directly. The
//! response carries the *remaining* lots (each a row, incl. provenance)
//! and the realized PnL; the frontend reconciles the lots against current
//! holdings and adds unrealized from the live spot.
//!
//! Solana deltas vs the Sui twin: no DeepBook — the `bm` param and the
//! order-fill legs are gone; auction wins replace secondary-market buys.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::handlers::buckets::strike_raw_to_usd;
use crate::ids;
use crate::state::{AppState, IndexedEvent, IndexerBucket};

/// f64 amounts below this (≈ sub-atomic) are treated as zero when
/// draining lots.
const EPS: f64 = 1e-9;

#[derive(Deserialize)]
pub struct PnlQuery {
    pub wallet: String,
}

#[derive(Serialize)]
pub struct LotDto {
    /// Underlying tokens still held from this lot (display units).
    pub amount: f64,
    /// Cost basis attributed to `amount` (settlement display units).
    pub cost: f64,
    /// `quote` (RFQ write) | `auction` — provenance, for the row label.
    pub source: &'static str,
    pub acquired_at_ms: i64,
}

#[derive(Serialize)]
pub struct BucketPnlDto {
    pub bucket_id: String,
    pub asset_decimals: u8,
    pub settlement_decimals: u8,
    /// FIFO lots still open after all tracked disposals, oldest-first.
    pub remaining_lots: Vec<LotDto>,
    /// Realized PnL from tracked disposals (settlement display units,
    /// signed).
    pub realized_pnl: f64,
    /// Exercised tokens we couldn't price. With the strike fallback this
    /// stays 0.0 in practice; kept for shape parity with the Sui twin.
    pub unpriced_exercise_amount: f64,
}

#[derive(Serialize)]
pub struct PnlResponse {
    pub buckets: Vec<BucketPnlDto>,
}

/// Proceeds of a disposal, before display-scaling.
enum Proceeds {
    /// Raw settlement smallest-units (burn = 0).
    SettlementRaw(f64),
    /// Real settlement-per-token mark (exercise); `None` ⇒ unpriced.
    PerToken(Option<f64>),
}

/// One balance-changing action against a bucket; amounts are raw on-chain
/// units (scaled to display by the caller, which knows the decimals).
enum Action {
    Acquire {
        amount_raw: f64,
        cost_raw: f64,
        source: &'static str,
        ts: i64,
    },
    Dispose {
        amount_raw: f64,
        proceeds: Proceeds,
    },
}

struct Ledger {
    lots: VecDeque<LotDto>,
    realized: f64,
    unpriced: f64,
    asset_decimals: u8,
    settlement_decimals: u8,
}

impl Ledger {
    fn acquire(&mut self, amount: f64, cost: f64, source: &'static str, ts: i64) {
        if amount <= EPS {
            return;
        }
        self.lots.push_back(LotDto {
            amount,
            cost,
            source,
            acquired_at_ms: ts,
        });
    }

    /// Consume `amount` oldest-first; realize `proceeds − consumed_cost`.
    /// Any amount beyond the tracked lots is a transferred-in ($0-cost)
    /// token being disposed — its share of proceeds is realized against
    /// zero cost. A `None` proceeds (unpriced exercise) consumes the lots
    /// break-even and is counted separately.
    fn dispose(&mut self, amount: f64, proceeds: Option<f64>) {
        let mut remaining = amount;
        let mut consumed_cost = 0.0;
        while remaining > EPS {
            let Some(front) = self.lots.front_mut() else {
                break;
            };
            let take = remaining.min(front.amount);
            let cost_take = if front.amount > EPS {
                front.cost * (take / front.amount)
            } else {
                0.0
            };
            front.amount -= take;
            front.cost -= cost_take;
            consumed_cost += cost_take;
            remaining -= take;
            if front.amount <= EPS {
                self.lots.pop_front();
            }
        }
        match proceeds {
            Some(p) => self.realized += p - consumed_cost,
            None => self.unpriced += amount,
        }
    }
}

fn pstr<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(|v| v.as_str())
}

fn pu64(payload: &Value, field: &str) -> Option<u64> {
    pstr(payload, field).and_then(|s| s.parse().ok())
}

pub async fn dashboard_pnl(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PnlQuery>,
) -> Result<Json<PnlResponse>, StatusCode> {
    if !ids::is_pubkey(&q.wallet) {
        return Ok(Json(PnlResponse { buckets: vec![] }));
    }

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

    // Already sequence-ascending — FIFO sees true chain order.
    let stream = state
        .indexer
        .events_for_participant(&q.wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer participant query failed");
            StatusCode::BAD_GATEWAY
        })?;

    // Pre-price exercises (one price-charting lookup each, best-effort)
    // before the sync fold. Fallback mark = the bucket strike in
    // settlement display units per token.
    let mut exercise_marks: HashMap<u64, Option<f64>> = HashMap::new();
    for ev in &stream {
        if !matches!(ev.event_type.as_str(), "Exercised" | "PutExercised") {
            continue;
        }
        if pstr(&ev.payload, "exerciser") != Some(q.wallet.as_str()) {
            continue;
        }
        let Some(bucket) = pstr(&ev.payload, "bucket").and_then(|b| buckets.get(b)) else {
            continue;
        };
        let mark = match price_at(&state, &bucket.option_mint, ev.timestamp_ms as i64).await {
            Some(p) => Some(p),
            None => strike_mark(&state, bucket),
        };
        exercise_marks.insert(ev.sequence, mark);
    }

    let mut ledgers: BTreeMap<String, Ledger> = BTreeMap::new();
    for ev in &stream {
        let Some((bucket_id, action)) = classify(ev, &q.wallet, &exercise_marks) else {
            continue;
        };
        // Lazily open the ledger; skip buckets with unknown token decimals.
        if !ledgers.contains_key(&bucket_id) {
            let Some(b) = buckets.get(&bucket_id) else {
                continue;
            };
            let (Some(asset), Some(settle)) = (
                state.catalog.lookup(&b.underlying_mint),
                state.catalog.lookup(&b.settlement_mint),
            ) else {
                continue;
            };
            ledgers.insert(
                bucket_id.clone(),
                Ledger {
                    lots: VecDeque::new(),
                    realized: 0.0,
                    unpriced: 0.0,
                    asset_decimals: asset.decimals,
                    settlement_decimals: settle.decimals,
                },
            );
        }
        let l = ledgers.get_mut(&bucket_id).expect("inserted above");
        let (ad, sd) = (l.asset_decimals, l.settlement_decimals);
        match action {
            Action::Acquire {
                amount_raw,
                cost_raw,
                source,
                ts,
            } => l.acquire(scale(amount_raw, ad), scale(cost_raw, sd), source, ts),
            Action::Dispose {
                amount_raw,
                proceeds,
            } => {
                let amount = scale(amount_raw, ad);
                let proceeds = match proceeds {
                    Proceeds::SettlementRaw(raw) => Some(scale(raw, sd)),
                    Proceeds::PerToken(mark) => mark.map(|m| m * amount),
                };
                l.dispose(amount, proceeds);
            }
        }
    }

    let buckets_out = ledgers
        .into_iter()
        .map(|(bucket_id, l)| BucketPnlDto {
            bucket_id,
            asset_decimals: l.asset_decimals,
            settlement_decimals: l.settlement_decimals,
            remaining_lots: l.lots.into_iter().filter(|lot| lot.amount > EPS).collect(),
            realized_pnl: l.realized,
            unpriced_exercise_amount: l.unpriced,
        })
        .collect();
    Ok(Json(PnlResponse {
        buckets: buckets_out,
    }))
}

/// Classify a single event into (bucket, action) for this wallet. `None`
/// for events that don't move this wallet's owned-option position.
fn classify(
    ev: &IndexedEvent,
    wallet: &str,
    exercise_marks: &HashMap<u64, Option<f64>>,
) -> Option<(String, Action)> {
    let payload = &ev.payload;
    let is = |field: &str| pstr(payload, field) == Some(wallet);
    let bucket = || {
        pstr(payload, "bucket")
            .and_then(ids::non_zero)
            .map(str::to_string)
    };
    match ev.event_type.as_str() {
        // Quote-driven write — the option-token recipient grows owned
        // options at gross premium cost.
        "WriteExecuted" if is("call_token_recipient") => Some((
            bucket()?,
            Action::Acquire {
                amount_raw: pu64(payload, "write_amount")? as f64,
                cost_raw: pu64(payload, "gross_premium")? as f64,
                source: "quote",
                ts: ev.timestamp_ms as i64,
            },
        )),
        "PutWriteExecuted" if is("put_token_recipient") => Some((
            bucket()?,
            Action::Acquire {
                amount_raw: pu64(payload, "write_amount")? as f64,
                cost_raw: pu64(payload, "gross_premium")? as f64,
                source: "quote",
                ts: ev.timestamp_ms as i64,
            },
        )),
        // Won option auction (covered_call / cash_secured_put): the token
        // recipient acquires the bucket's option tokens at the gross bid.
        // Pure swaps carry a zero bucket and are skipped by `bucket()?`.
        "AuctionSettled" if is("token_recipient") => Some((
            bucket()?,
            Action::Acquire {
                amount_raw: pu64(payload, "amount")? as f64,
                cost_raw: pu64(payload, "gross_bid")? as f64,
                source: "auction",
                ts: ev.timestamp_ms as i64,
            },
        )),
        "Exercised" | "PutExercised" if is("exerciser") => Some((
            bucket()?,
            Action::Dispose {
                amount_raw: pu64(payload, "amount")? as f64,
                proceeds: Proceeds::PerToken(exercise_marks.get(&ev.sequence).copied().flatten()),
            },
        )),
        "ExpiredOptionBurned" | "PutExpiredOptionBurned" if is("burner") => Some((
            bucket()?,
            Action::Dispose {
                amount_raw: pu64(payload, "amount")? as f64,
                proceeds: Proceeds::SettlementRaw(0.0),
            },
        )),
        _ => None,
    }
}

/// The strike fallback mark: settlement display units per whole
/// underlying token. `None` if either mint is missing from the catalog
/// (in which case the ledger is skipped anyway).
fn strike_mark(state: &AppState, bucket: &IndexerBucket) -> Option<f64> {
    let u = state.catalog.lookup(&bucket.underlying_mint)?.decimals;
    let s = state.catalog.lookup(&bucket.settlement_mint)?.decimals;
    Some(strike_raw_to_usd(bucket.strike, bucket.strike_scale, u, s))
}

/// Fetch the option price at `ms` from solana-price-charting; `None` if
/// the service is unset/unreachable or had no data (always, until a
/// Solana DEX integration lands). Keyed by the bucket's option mint.
async fn price_at(state: &AppState, option_mint: &str, ms: i64) -> Option<f64> {
    let base = state.price_charting_url.as_deref()?;
    let url = format!("{base}/price-at?pool_id={option_mint}&ms={ms}");
    let resp = state
        .http
        .get(&url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let body: PriceAtBody = resp.json().await.ok()?;
    body.mid.or(body.close)
}

#[derive(Deserialize)]
struct PriceAtBody {
    mid: Option<f64>,
    close: Option<f64>,
}

fn scale(raw: f64, decimals: u8) -> f64 {
    raw / 10f64.powi(decimals as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ledger() -> Ledger {
        Ledger {
            lots: VecDeque::new(),
            realized: 0.0,
            unpriced: 0.0,
            asset_decimals: 0,
            settlement_decimals: 0,
        }
    }

    #[test]
    fn fifo_realizes_against_oldest_lot_first() {
        let mut l = ledger();
        l.acquire(10.0, 20.0, "quote", 1); // 10 @ $2
        l.acquire(10.0, 30.0, "auction", 2); // 10 @ $3
        // Sell 15 for $60 ($4/unit): consumes all of lot1 (cost 20) + 5 of
        // lot2 (cost 15) → consumed cost 35, realized 60 − 35 = 25.
        l.dispose(15.0, Some(60.0));
        assert!((l.realized - 25.0).abs() < 1e-9);
        // 5 left from lot2 at $3 → cost 15.
        assert_eq!(l.lots.len(), 1);
        let rem = l.lots.front().unwrap();
        assert!((rem.amount - 5.0).abs() < 1e-9);
        assert!((rem.cost - 15.0).abs() < 1e-9);
        assert_eq!(rem.source, "auction");
    }

    #[test]
    fn disposing_more_than_held_costs_zero_on_the_excess() {
        let mut l = ledger();
        l.acquire(5.0, 10.0, "quote", 1); // 5 @ $2
        // Sell 8 for $24: 5 tracked (cost 10) + 3 transferred-in ($0) →
        // realized 24 − 10 = 14, no lots left.
        l.dispose(8.0, Some(24.0));
        assert!((l.realized - 14.0).abs() < 1e-9);
        assert!(l.lots.is_empty());
    }

    #[test]
    fn unpriced_exercise_consumes_lots_without_realizing() {
        let mut l = ledger();
        l.acquire(10.0, 20.0, "quote", 1);
        l.dispose(4.0, None);
        assert_eq!(l.realized, 0.0);
        assert!((l.unpriced - 4.0).abs() < 1e-9);
        assert!((l.lots.front().unwrap().amount - 6.0).abs() < 1e-9);
    }

    #[test]
    fn burn_realizes_full_cost_as_loss() {
        let mut l = ledger();
        l.acquire(10.0, 25.0, "quote", 1);
        l.dispose(10.0, Some(0.0)); // expired worthless
        assert!((l.realized + 25.0).abs() < 1e-9);
        assert!(l.lots.is_empty());
    }

    // ── classify: auction-era event stream, canned payload JSON ──────────

    const WALLET: &str = "By1111111111111111111111111111111111111111";
    const BUCKET: &str = "Bk1111111111111111111111111111111111111111";

    fn ev(sequence: u64, event_type: &str, payload: Value) -> IndexedEvent {
        IndexedEvent {
            sequence,
            slot: 1,
            signature: "sig".to_string(),
            event_type: event_type.to_string(),
            timestamp_ms: 1_760_000_000_000,
            payload,
        }
    }

    #[test]
    fn classify_quote_write_and_auction_win_as_acquires() {
        let marks = HashMap::new();
        let w = ev(
            1,
            "WriteExecuted",
            json!({ "bucket": BUCKET, "call_token_recipient": WALLET,
                    "position_recipient": "Other", "write_amount": "100",
                    "gross_premium": "90", "net_premium": "85" }),
        );
        let (b, a) = classify(&w, WALLET, &marks).unwrap();
        assert_eq!(b, BUCKET);
        let Action::Acquire { amount_raw, cost_raw, source, .. } = a else {
            panic!("expected acquire");
        };
        assert_eq!(amount_raw, 100.0);
        assert_eq!(cost_raw, 90.0);
        assert_eq!(source, "quote");

        let s = ev(
            2,
            "AuctionSettled",
            json!({ "auction": "Auc1", "mode": "covered_call", "bucket": BUCKET,
                    "winner": WALLET, "token_recipient": WALLET,
                    "position_recipient": "Vault1", "amount": "50",
                    "gross_bid": "40", "fee": "2", "net_proceeds": "38" }),
        );
        let (_, a) = classify(&s, WALLET, &marks).unwrap();
        let Action::Acquire { amount_raw, cost_raw, source, .. } = a else {
            panic!("expected acquire");
        };
        assert_eq!(amount_raw, 50.0);
        assert_eq!(cost_raw, 40.0);
        assert_eq!(source, "auction");

        // Swap auctions (zero bucket) never touch the option ledger.
        let swap = ev(
            3,
            "AuctionSettled",
            json!({ "auction": "Auc2", "mode": "swap", "bucket": ids::ZERO_PUBKEY,
                    "winner": WALLET, "token_recipient": WALLET,
                    "position_recipient": ids::ZERO_PUBKEY, "amount": "10",
                    "gross_bid": "9", "fee": "0", "net_proceeds": "9" }),
        );
        assert!(classify(&swap, WALLET, &marks).is_none());
    }

    #[test]
    fn classify_exercise_uses_precomputed_mark_and_burn_is_zero() {
        let mut marks = HashMap::new();
        marks.insert(4u64, Some(2.5));
        let e = ev(
            4,
            "Exercised",
            json!({ "bucket": BUCKET, "exerciser": WALLET, "amount": "40",
                    "settlement_paid": "3400", "cursor_after": "40" }),
        );
        let (_, a) = classify(&e, WALLET, &marks).unwrap();
        let Action::Dispose { amount_raw, proceeds: Proceeds::PerToken(mark) } = a else {
            panic!("expected per-token dispose");
        };
        assert_eq!(amount_raw, 40.0);
        assert_eq!(mark, Some(2.5));

        let b = ev(
            5,
            "ExpiredOptionBurned",
            json!({ "bucket": BUCKET, "burner": WALLET, "amount": "7" }),
        );
        let (_, a) = classify(&b, WALLET, &marks).unwrap();
        let Action::Dispose { proceeds: Proceeds::SettlementRaw(p), .. } = a else {
            panic!("expected settlement dispose");
        };
        assert_eq!(p, 0.0);

        // Someone else's exercise is not ours.
        let other = ev(
            6,
            "Exercised",
            json!({ "bucket": BUCKET, "exerciser": "Other", "amount": "1" }),
        );
        assert!(classify(&other, WALLET, &marks).is_none());
    }

    #[test]
    fn end_to_end_ledger_write_then_burn() {
        // 100 raw @ decimals 0, cost 90 raw @ decimals 0; burn all →
        // realized −90.
        let mut l = ledger();
        l.acquire(100.0, 90.0, "quote", 1);
        l.dispose(100.0, Some(0.0));
        assert!((l.realized + 90.0).abs() < 1e-9);
    }
}
