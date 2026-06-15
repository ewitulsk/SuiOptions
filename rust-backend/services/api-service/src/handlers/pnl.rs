//! `GET /dashboard/pnl?wallet=0x…&bm=0x…` — true options PnL (SO-209).
//!
//! Replaces the dashboard's pro-rated `(intrinsic − premium)` estimate with a
//! per-(wallet, bucket) **FIFO cost-lot ledger**. Acquisitions push lots,
//! disposals consume them oldest-first and accrue realized PnL:
//!
//! - Acquire: `WriteExecuted` (RFQ mint, cost = gross premium) and DeepBook
//!   `OrderFilled` buys (cost = quote + settlement-denominated taker/maker fee).
//! - Dispose: DeepBook `OrderFilled` sells (proceeds = quote − fee), `Exercised`
//!   (proceeds marked at the option-pool price at exercise time via
//!   price-charting), `ExpiredOptionBurned` (proceeds = 0, a realized loss).
//!
//! Everything is computed on demand in **display units** (underlying tokens /
//! settlement USD) so the frontend renders it directly. The response carries
//! the *remaining* lots (each a row, incl. provenance) and the realized PnL;
//! the frontend reconciles the lots against current holdings (adding $0-cost
//! rows for transferred-in tokens) and adds unrealized from the live spot.
//!
//! DeepBook fills are attributed by BalanceManager id — the frontend passes its
//! own `bm`, so no BM→owner table is needed (a BM id only ever appears as a
//! fill participant in the event log).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use protocol_types::events::{ChainEvent, IndexedEvent};
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::state::AppState;

/// f64 amounts below this (≈ sub-atomic) are treated as zero when draining lots.
const EPS: f64 = 1e-9;

#[derive(Deserialize)]
pub struct PnlQuery {
    pub wallet: String,
    /// The wallet's DeepBook BalanceManager id. Optional — omitted when the
    /// user has never enabled trading, in which case there are no fills.
    pub bm: Option<String>,
}

#[derive(Serialize)]
pub struct LotDto {
    /// Underlying tokens still held from this lot (display units).
    pub amount: f64,
    /// Cost basis attributed to `amount` (settlement display units, e.g. USDC).
    pub cost: f64,
    /// `rfq` | `deepbook` — provenance, for the card's row label.
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
    /// Realized PnL from tracked disposals (settlement display units, signed).
    pub realized_pnl: f64,
    /// Exercised tokens we couldn't price (no option-pool data at exercise ts).
    /// Consumed from lots but excluded from `realized_pnl`; surfaced so the UI
    /// can flag the gap rather than silently under-report.
    pub unpriced_exercise_amount: f64,
}

#[derive(Serialize)]
pub struct PnlResponse {
    pub buckets: Vec<BucketPnlDto>,
}

/// Proceeds of a disposal, before display-scaling. The two forms differ in
/// units: secondary-market sells/burns carry a raw settlement amount, while an
/// exercise is marked per underlying token (a real option price).
enum Proceeds {
    /// Raw settlement smallest-units (DeepBook sell quote − fee; burn = 0).
    SettlementRaw(f64),
    /// Real settlement-per-token mark (exercise); `None` ⇒ unpriced.
    PerToken(Option<f64>),
}

/// One balance-changing action against a bucket; amounts are raw on-chain units
/// (scaled to display by the caller, which knows the bucket's decimals).
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

    /// Consume `amount` oldest-first; realize `proceeds − consumed_cost`. Any
    /// amount beyond the tracked lots is a transferred-in ($0-cost) token being
    /// disposed — its share of proceeds is realized against zero cost. A `None`
    /// proceeds (unpriced exercise) consumes the lots break-even and is counted
    /// separately.
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

pub async fn dashboard_pnl(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PnlQuery>,
) -> Result<Json<PnlResponse>, StatusCode> {
    let Ok(wallet) = SuiAddress::from_hex(&q.wallet) else {
        return Ok(Json(PnlResponse { buckets: vec![] }));
    };
    let bm = q.bm.as_deref().and_then(|s| ObjectId::from_hex(s).ok());

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

    let wallet_events = state
        .indexer
        .events_for_participant(wallet)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer participant query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let fills = match bm {
        Some(bm) => state.indexer.deepbook_fills_for_bm(bm).await.map_err(|e| {
            tracing::warn!(error = %e, "indexer fills query failed");
            StatusCode::BAD_GATEWAY
        })?,
        None => Vec::new(),
    };

    // Merge into one sequence-ordered stream so FIFO sees true chain order.
    let mut stream: Vec<IndexedEvent> = Vec::with_capacity(wallet_events.len() + fills.len());
    stream.extend(wallet_events);
    stream.extend(fills);
    stream.sort_by_key(|e| e.sequence);

    // Pre-price exercises (one price-charting lookup each) before the sync fold.
    let mut exercise_marks: HashMap<u64, Option<f64>> = HashMap::new();
    for ev in &stream {
        if let ChainEvent::Exercised(e) = &ev.event {
            if e.exerciser != wallet {
                continue;
            }
            let mark = match buckets.get(&e.bucket_id).and_then(|b| b.deepbook_pool_id) {
                Some(pool) => price_at(&state, &pool.to_hex(), ev.timestamp_ms as i64).await,
                None => None,
            };
            exercise_marks.insert(ev.sequence, mark);
        }
    }

    let mut ledgers: BTreeMap<ObjectId, Ledger> = BTreeMap::new();
    for ev in &stream {
        let Some((bucket_id, action)) = classify(&ev.event, &wallet, bm, ev, &exercise_marks)
        else {
            continue;
        };
        // Lazily open the ledger; skip buckets with unknown token decimals.
        if !ledgers.contains_key(&bucket_id) {
            let Some(b) = buckets.get(&bucket_id) else {
                continue;
            };
            let (Some(asset), Some(settle)) = (
                state.catalog.lookup(b.asset_type.as_str()),
                state.catalog.lookup(b.settlement_type.as_str()),
            ) else {
                continue;
            };
            ledgers.insert(
                bucket_id,
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
            bucket_id: bucket_id.to_hex(),
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

/// Classify a single event into (bucket, action) for this wallet/BM. `None` for
/// events that don't move this wallet's owned-call position.
fn classify(
    event: &ChainEvent,
    wallet: &SuiAddress,
    bm: Option<ObjectId>,
    ev: &IndexedEvent,
    exercise_marks: &HashMap<u64, Option<f64>>,
) -> Option<(ObjectId, Action)> {
    match event {
        // RFQ purchase — the only WriteExecuted role that grows owned calls.
        ChainEvent::WriteExecuted(w) if w.call_token_recipient == *wallet => Some((
            w.bucket_id,
            Action::Acquire {
                amount_raw: w.write_amount as f64,
                cost_raw: w.gross_premium as f64,
                source: "rfq",
                ts: ev.timestamp_ms as i64,
            },
        )),
        ChainEvent::Exercised(e) if e.exerciser == *wallet => Some((
            e.bucket_id,
            Action::Dispose {
                amount_raw: e.amount as f64,
                proceeds: Proceeds::PerToken(exercise_marks.get(&ev.sequence).copied().flatten()),
            },
        )),
        ChainEvent::ExpiredOptionBurned(b) if b.burner == *wallet => Some((
            b.bucket_id,
            Action::Dispose {
                amount_raw: b.amount as f64,
                proceeds: Proceeds::SettlementRaw(0.0),
            },
        )),
        ChainEvent::DeepBookOrderFilled(f) => {
            let bm = bm?;
            let is_taker = f.taker_balance_manager_id == bm;
            let is_maker = f.maker_balance_manager_id == bm;
            if !is_taker && !is_maker {
                return None;
            }
            // Bought = on the bid side: taker_is_bid ⇒ taker bought; else maker bought.
            let user_bought = if is_taker {
                f.taker_is_bid
            } else {
                !f.taker_is_bid
            };
            let fee_is_deep = if is_taker {
                f.taker_fee_is_deep
            } else {
                f.maker_fee_is_deep
            };
            let fee_raw = if is_taker { f.taker_fee } else { f.maker_fee };
            // Only settlement-denominated fees touch the cost basis (decision:
            // include the USDC fee; DEEP-denominated fees are a separate asset).
            let fee = if fee_is_deep { 0.0 } else { fee_raw as f64 };
            let amount_raw = f.base_quantity as f64;
            if user_bought {
                Some((
                    f.bucket_id,
                    Action::Acquire {
                        amount_raw,
                        cost_raw: f.quote_quantity as f64 + fee,
                        source: "deepbook",
                        ts: f.timestamp_ms as i64,
                    },
                ))
            } else {
                Some((
                    f.bucket_id,
                    Action::Dispose {
                        amount_raw,
                        proceeds: Proceeds::SettlementRaw((f.quote_quantity as f64 - fee).max(0.0)),
                    },
                ))
            }
        }
        _ => None,
    }
}

/// Fetch the option-pool price at `ms` from price-charting; `None` if the
/// service is unset/unreachable or had no data. Prefers the order-book mid
/// (continuous), falling back to the last trade.
async fn price_at(state: &AppState, pool_id: &str, ms: i64) -> Option<f64> {
    let base = state.price_charting_url.as_deref()?;
    let url = format!("{base}/price-at?pool_id={pool_id}&ms={ms}");
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
        l.acquire(10.0, 20.0, "rfq", 1); // 10 @ $2
        l.acquire(10.0, 30.0, "deepbook", 2); // 10 @ $3
                                              // Sell 15 for $60 ($4/unit): consumes all of lot1 (cost 20) + 5 of lot2
                                              // (cost 15) → consumed cost 35, realized 60 − 35 = 25.
        l.dispose(15.0, Some(60.0));
        assert!((l.realized - 25.0).abs() < 1e-9);
        // 5 left from lot2 at $3 → cost 15.
        assert_eq!(l.lots.len(), 1);
        let rem = l.lots.front().unwrap();
        assert!((rem.amount - 5.0).abs() < 1e-9);
        assert!((rem.cost - 15.0).abs() < 1e-9);
    }

    #[test]
    fn disposing_more_than_held_costs_zero_on_the_excess() {
        let mut l = ledger();
        l.acquire(5.0, 10.0, "rfq", 1); // 5 @ $2
                                        // Sell 8 for $24: 5 tracked (cost 10) + 3 transferred-in ($0) →
                                        // realized 24 − 10 = 14, no lots left.
        l.dispose(8.0, Some(24.0));
        assert!((l.realized - 14.0).abs() < 1e-9);
        assert!(l.lots.is_empty());
    }

    #[test]
    fn unpriced_exercise_consumes_lots_without_realizing() {
        let mut l = ledger();
        l.acquire(10.0, 20.0, "rfq", 1);
        l.dispose(4.0, None);
        assert_eq!(l.realized, 0.0);
        assert!((l.unpriced - 4.0).abs() < 1e-9);
        assert!((l.lots.front().unwrap().amount - 6.0).abs() < 1e-9);
    }

    #[test]
    fn burn_realizes_full_cost_as_loss() {
        let mut l = ledger();
        l.acquire(10.0, 25.0, "rfq", 1);
        l.dispose(10.0, Some(0.0)); // expired worthless
        assert!((l.realized + 25.0).abs() < 1e-9);
        assert!(l.lots.is_empty());
    }
}
