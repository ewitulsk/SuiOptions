//! REST handlers (spec §5.3). All responses include server time; market
//! config rides on `/v1/markets`.

use crate::intake;
use crate::ladders::ladders_for_market;
use crate::state::{now_ms, AppState};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use orderbook_book::PlaceOutcome;
use orderbook_core::order::{SignatureScheme, SignedOrder};
use orderbook_core::{Digest, SuiAddress};
use orderbook_settlement::MatchJob;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub type ApiResult = Result<Json<Value>, ApiError>;

pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub detail: String,
}

impl ApiError {
    fn bad(code: &str, detail: impl Into<String>) -> Self {
        ApiError { status: StatusCode::BAD_REQUEST, code: code.into(), detail: detail.into() }
    }
    fn not_found(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND".into(),
            detail: detail.into(),
        }
    }
    fn internal(detail: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL".into(),
            detail: detail.into(),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "detail": self.detail } })),
        )
            .into_response()
    }
}

impl From<orderbook_store::StoreError> for ApiError {
    fn from(e: orderbook_store::StoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}

fn b64(s: &str) -> Result<Vec<u8>, ApiError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| ApiError::bad("BAD_BASE64", e.to_string()))
}

// === Markets / book / trades ===

pub async fn markets(State(state): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "markets": state.markets,
    })))
}

#[derive(Deserialize)]
pub struct BookQuery {
    #[serde(default = "default_depth")]
    depth: usize,
}
fn default_depth() -> usize {
    20
}

pub async fn book(
    State(state): State<Arc<AppState>>,
    Path(market): Path<String>,
    Query(q): Query<BookQuery>,
) -> ApiResult {
    let m = state
        .resolve_market(&market)
        .ok_or_else(|| ApiError::not_found("unknown market"))?;
    let book = state
        .book(&m.registry_id)
        .ok_or_else(|| ApiError::internal("book missing"))?;
    let (bids, asks) = book.lock().snapshot(q.depth.min(200));
    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "market": m.registry_id.to_hex(),
        "tickSize": m.tick_size,
        "lotSize": m.lot_size,
        "bids": bids,
        "asks": asks,
    })))
}

pub async fn trades(
    State(state): State<Arc<AppState>>,
    Path(market): Path<String>,
) -> ApiResult {
    let m = state
        .resolve_market(&market)
        .ok_or_else(|| ApiError::not_found("unknown market"))?;
    let trades = state.store.recent_trades(&m.registry_id, 100).await?;
    Ok(Json(json!({ "serverTimeMs": now_ms(), "trades": trades })))
}

/// Open-orderbook mode: this response IS the fill ticket (§5.3).
pub async fn order_by_digest(
    State(state): State<Arc<AppState>>,
    Path((market, digest)): Path<(String, String)>,
) -> ApiResult {
    let m = state
        .resolve_market(&market)
        .ok_or_else(|| ApiError::not_found("unknown market"))?;
    let digest =
        Digest::parse(&digest).map_err(|e| ApiError::bad("BAD_DIGEST", e.to_string()))?;
    let stored = state
        .store
        .get_order(&digest)
        .await?
        .ok_or_else(|| ApiError::not_found("unknown order"))?;
    if stored.signed.registry_id != m.registry_id {
        return Err(ApiError::not_found("order not in this market"));
    }
    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "digest": digest.to_hex(),
        "status": stored.status,
        "filledTaker": stored.filled_taker.to_string(),
        "order": stored.signed,
    })))
}

// === Order placement (§5.4) ===

pub async fn place_order(
    State(state): State<Arc<AppState>>,
    Json(signed): Json<SignedOrder>,
) -> ApiResult {
    let (digest, _side, _price) = intake::intake_order(&state, &signed)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: format!("{:?}", e.code),
            detail: e.detail,
        })?;

    let market = state
        .market(&signed.registry_id)
        .ok_or_else(|| ApiError::internal("market vanished"))?
        .clone();
    let book = state
        .book(&market.registry_id)
        .ok_or_else(|| ApiError::internal("book missing"))?;

    let (outcome, intents) = {
        let mut b = book.lock();
        b.place(digest, &signed.order)
            .map_err(|e| ApiError::bad("BOOK_REJECT", e.to_string()))?
    };

    // resolve intents into settlement jobs
    for intent in &intents {
        let ask = state.store.get_order(&intent.ask_digest).await?;
        let bid = state.store.get_order(&intent.bid_digest).await?;
        if let (Some(ask), Some(bid)) = (ask, bid) {
            let job = MatchJob {
                intent: intent.clone(),
                ask,
                bid,
                base_type: market.base.clone(),
                quote_type: market.quote.clone(),
            };
            if state.match_tx.send(job).await.is_err() {
                tracing::error!(alert_id = "tx-failed-match-queue", "settlement queue closed");
            }
        }
    }

    // WS: ack to the maker, delta to the market channel
    state.publish(
        format!("orders.{}", signed.order.maker.to_hex()),
        json!({ "type": "ack", "digest": digest.to_hex(), "outcome": outcome_str(&outcome) }),
    );
    state.publish_book_snapshot(&market);

    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "digest": digest.to_hex(),
        "status": outcome_str(&outcome),
        "matches": intents.len(),
    })))
}

fn outcome_str(o: &PlaceOutcome) -> &'static str {
    match o {
        PlaceOutcome::Matched => "MATCHED",
        PlaceOutcome::Rested { .. } => "OPEN",
        PlaceOutcome::SelfTradeCancelled => "SELF_TRADE_CANCELLED",
    }
}

// === Soft cancel (§4.7 tier 1) ===

/// Domain prefix for the signed soft-cancel payload: the maker (or an
/// approved signer) signs `personal_message(TAG ‖ digest_bytes)`.
pub const CANCEL_DOMAIN_TAG: &[u8] = b"SUI_HYBRID_EXCHANGE_CANCEL";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelRequest {
    scheme: SignatureScheme,
    /// base64 raw 64-byte signature
    signature: String,
    /// base64 public key
    public_key: String,
}

pub async fn cancel_order(
    State(state): State<Arc<AppState>>,
    Path(digest): Path<String>,
    Json(req): Json<CancelRequest>,
) -> ApiResult {
    let digest =
        Digest::parse(&digest).map_err(|e| ApiError::bad("BAD_DIGEST", e.to_string()))?;
    let stored = state
        .store
        .get_order(&digest)
        .await?
        .ok_or_else(|| ApiError::not_found("unknown order"))?;

    let mut message = CANCEL_DOMAIN_TAG.to_vec();
    message.extend_from_slice(&digest.0);
    let sig = b64(&req.signature)?;
    let pk = b64(&req.public_key)?;
    let derived = orderbook_signing::verify_signature(req.scheme, &message, &sig, &pk)
        .map_err(|e| ApiError::bad("BAD_SIGNATURE", e.to_string()))?;
    let authorized = derived == stored.signed.order.maker
        || state
            .store
            .is_approved_signer(&stored.signed.order.maker_manager_id, &derived)
            .await?;
    if !authorized {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            code: "NOT_MAKER".into(),
            detail: "signer is not the maker or an approved signer".into(),
        });
    }

    state
        .store
        .set_order_status(&digest, orderbook_store::OrderStatus::Cancelled)
        .await?;
    if let Some(m) = state.market(&stored.signed.registry_id) {
        let m = m.clone();
        if let Some(book) = state.book(&m.registry_id) {
            book.lock().remove(&digest);
        }
        state.publish_book_snapshot(&m);
    }
    state.publish(
        format!("orders.{}", stored.signed.order.maker.to_hex()),
        json!({ "type": "cancelled", "digest": digest.to_hex(), "soft": true }),
    );

    // Honest caveat (spec §4.7): a soft-cancelled open order remains
    // technically fillable on-chain by anyone who saved the signature.
    let unrestricted = stored.signed.order.sender.is_zero();
    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "digest": digest.to_hex(),
        "status": "CANCELLED",
        "softCancelOnly": true,
        "stillFillableOnChain": unrestricted,
        "hint": if unrestricted {
            "sender is unrestricted: hard-cancel on chain for certainty"
        } else {
            "sender is pinned: only the pinned relayer could settle this"
        },
    })))
}

// === Accounts ===

pub async fn account_orders(
    State(state): State<Arc<AppState>>,
    Path(addr): Path<String>,
) -> ApiResult {
    let addr = SuiAddress::parse(&addr).map_err(|e| ApiError::bad("BAD_ADDRESS", e.to_string()))?;
    let orders = state.store.orders_by_account(&addr).await?;
    Ok(Json(json!({ "serverTimeMs": now_ms(), "orders": orders })))
}

pub async fn account_fills(
    State(state): State<Arc<AppState>>,
    Path(addr): Path<String>,
) -> ApiResult {
    let addr = SuiAddress::parse(&addr).map_err(|e| ApiError::bad("BAD_ADDRESS", e.to_string()))?;
    let fills = state.store.fills_by_account(&addr).await?;
    Ok(Json(json!({ "serverTimeMs": now_ms(), "fills": fills })))
}

/// Escrow balances by manager ID (mirrored from chain events).
pub async fn account_balance(
    State(state): State<Arc<AppState>>,
    Path(addr): Path<String>,
) -> ApiResult {
    let addr = SuiAddress::parse(&addr).map_err(|e| ApiError::bad("BAD_ADDRESS", e.to_string()))?;
    let balances = state.store.balances_of(&addr).await?;
    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "manager": addr.to_hex(),
        "balances": balances
            .into_iter()
            .map(|(token, amount)| json!({ "token": token, "amount": amount.to_string() }))
            .collect::<Vec<_>>(),
    })))
}

// === Split-route quotes (§5.8) ===

#[derive(Deserialize)]
pub struct RouteQuery {
    from: String,
    to: String,
    amount: u64,
}

pub async fn routes(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RouteQuery>,
) -> ApiResult {
    let from = orderbook_core::canonicalize_move_type(&q.from)
        .map_err(|e| ApiError::bad("BAD_TOKEN", e.to_string()))?;
    let to = orderbook_core::canonicalize_move_type(&q.to)
        .map_err(|e| ApiError::bad("BAD_TOKEN", e.to_string()))?;

    let mut all_ladders = Vec::new();
    for m in &state.markets {
        if let Some(book) = state.book(&m.registry_id) {
            all_ladders.extend(ladders_for_market(m, &book.lock()));
        }
    }
    let plan = orderbook_router::plan_route(
        &all_ladders,
        &from,
        &to,
        q.amount,
        orderbook_router::RouterConfig::default(),
    )
    .map_err(|e| ApiError::bad("NO_ROUTE", e.to_string()))?;

    // attach the signed orders (fill tickets) and a PTB skeleton
    let mut orders = serde_json::Map::new();
    let mut skeleton = Vec::new();
    for path in &plan.paths {
        for (hop_idx, hop) in path.hops.iter().enumerate() {
            let market_id = path.markets[hop_idx];
            let m = state
                .market(&market_id)
                .ok_or_else(|| ApiError::internal("market missing"))?;
            let hop_from = &path.tokens[hop_idx];
            for leg in hop {
                let stored = state
                    .store
                    .get_order(&leg.digest)
                    .await?
                    .ok_or_else(|| ApiError::internal("routed order missing"))?;
                orders.insert(
                    leg.digest.to_hex(),
                    serde_json::to_value(&stored.signed).unwrap_or(Value::Null),
                );
                // paying quote into an ask => fill_limit_order; paying base
                // into a bid => fill_limit_order_reverse
                let function = if hop_from == &m.quote {
                    "fill_limit_order"
                } else {
                    "fill_limit_order_reverse"
                };
                skeleton.push(json!({
                    "command": function,
                    "market": market_id.to_hex(),
                    "typeArgs": [m.base, m.quote],
                    "digest": leg.digest.to_hex(),
                    "amountIn": leg.amount_in.to_string(),
                    // intra-route hops use 0; ONE strict guard at the end
                    "minMakerAmountOut": "0",
                }));
            }
        }
    }
    skeleton.push(json!({
        "command": "assert_coin_min",
        "typeArgs": [to],
        "min": plan.expected_out.to_string(),
        "note": "single strict route min-out; adjust for acceptable slippage",
    }));

    Ok(Json(json!({
        "serverTimeMs": now_ms(),
        "plan": plan,
        "orders": orders,
        "ptbSkeleton": skeleton,
    })))
}
