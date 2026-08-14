//! Curated trading vault endpoints (SO-287):
//!
//!   - `GET /trading-vaults`     — list with headline state / observed pps
//!   - `GET /trading-vaults/:id` — one vault + its adapter positions (past
//!     positions included, `active=false`)
//!
//! Event-derived analytics (SO-293):
//!
//!   - `GET /trading-vaults/:id/pps-history`     — observed pps points from
//!     TvDeposited / TvWithdrawFulfilled events, ascending by time
//!   - `GET /trading-vaults/:id/stake/:address`  — one wallet's live stake
//!     replayed from TvDeposited / TvWithdrawRequested events
//!   - `GET /trading-vaults/:id/trades`          — curator spot trades from
//!     TvTakerSwapExecuted events, newest first (SO-313)
//!   - `GET /trading-vaults/:id/pending-requests` — outstanding withdraw
//!     queue with per-request payout assets (SO-370)
//!
//! All reads are JIT GraphQL queries to the indexer, except the detail
//! endpoint's `balances[]` — free balances are stated by no event, so they
//! come from a live Sui object read (`sui_rpc::fetch_vault_balances`) that
//! degrades to `balances_stale` on failure. Balance-precise NAV still isn't
//! served here.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::json;

use indexer_graphql::TradingVault;
use protocol_types::events::ChainEvent;
use protocol_types::ids::{ObjectId, SuiAddress};

use crate::state::AppState;
use crate::sui_rpc;

/// pps is a 1e12-scaled accounting-asset-per-share.
const PPS_SCALE: f64 = 1e12;

/// pps scale as an integer, for exact event-derived arithmetic.
const PPS_E12: u128 = 1_000_000_000_000;

/// Mirror of the Move-side `SHARE_OFFSET` constant in
/// `contracts/trading-vault/sources/vault.move` (SO-370): a genesis deposit
/// of V accounting units mints V × 1e6 shares, so event-derived share prices
/// (`value / shares`) must be rescaled by this factor for genesis pps to
/// come out as `PPS_E12`.
const SHARE_OFFSET: u128 = 1_000_000;

/// Cap on the per-vault event scans backing the analytics endpoints. The
/// indexer serves the most recent events first, so a vault with more matching
/// events than this silently loses its OLDEST history (earliest pps points /
/// earliest stake flows), not the newest.
const EVENT_SCAN_CAP: usize = 5000;

#[derive(Serialize)]
pub struct TradingVaultDto {
    pub vault_id: String,
    /// The vault's unit of account (renamed from deposit_* in SO-370:
    /// deposits may arrive in any allowlisted asset).
    pub accounting_symbol: String,
    pub accounting_decimals: Option<u8>,
    pub accounting_coin_type: String,
    pub creator: String,
    /// Current curator wallet (updated on curator rotation).
    pub curator: String,
    pub curator_cap_id: String,
    /// open | closing | closed.
    pub state: String,
    pub lockup_ms: i64,
    pub curator_fee_bps: i64,
    pub unwind_grace_ms: i64,
    pub deposits_paused: bool,
    pub mm_release_enabled: bool,
    pub total_shares_raw: String,
    pub position_count: i64,
    pub pending_withdrawals: i64,
    /// Observed accounting-asset-per-share at the last deposit/withdraw
    /// (PPS_SCALE-adjusted).
    pub pps: Option<f64>,
    pub pps_raw: Option<String>,
    pub updated_at_ms: i64,
    /// External MM account wallet (SO-299); null when none is set.
    pub external_account: Option<String>,
    /// Outstanding external exposure (accounting-asset units), decimal string.
    pub external_exposure: String,
    /// Latest keeper-posted account equity, decimal string.
    pub latest_external_equity: Option<String>,
    pub external_equity_updated_at_ms: Option<String>,
    /// NAV from the latest consumed appraisal (accounting-asset units,
    /// decimal string; SO-304). Null before the first appraisal.
    pub latest_nav_raw: Option<String>,
    pub nav_updated_at_ms: Option<i64>,
}

#[derive(Serialize)]
pub struct TradingVaultsResponse {
    pub vaults: Vec<TradingVaultDto>,
}

#[derive(Serialize)]
pub struct TradingVaultPositionDto {
    pub position_id: String,
    pub adapter: String,
    pub active: bool,
    pub stored_at_ms: i64,
    pub removed_at_ms: Option<i64>,
    /// Latest appraisal mark (accounting-asset units, decimal string;
    /// SO-304). Null until the position is first appraised.
    pub last_value_raw: Option<String>,
    pub last_appraised_at_ms: Option<i64>,
}

/// One free balance the vault holds outside custody (SO-313) — a
/// `vault::BalanceKey<T>` dynamic field. The deposit asset is included; the
/// UI decides how to style it.
#[derive(Serialize)]
pub struct TradingVaultBalanceDto {
    /// Canonical `0x…::mod::T` coin type.
    pub coin_type: String,
    /// Catalog symbol, falling back to the coin type when unknown.
    pub symbol: String,
    /// Catalog decimals; null when the asset isn't in the catalog.
    pub decimals: Option<u8>,
    /// Raw u64 amount in atomic units, decimal string (consistent with the
    /// other raw integer fields in this handler).
    pub amount_raw: String,
}

#[derive(Serialize)]
pub struct TradingVaultDetailResponse {
    #[serde(flatten)]
    pub vault: TradingVaultDto,
    pub positions: Vec<TradingVaultPositionDto>,
    /// Free balances read live off the vault object. Empty when the RPC read
    /// fails — `balances_stale` says which of the two it is.
    pub balances: Vec<TradingVaultBalanceDto>,
    /// True when the live balance read failed and `balances` is therefore
    /// unknown rather than genuinely empty. Callers must not render an empty
    /// `balances` as "holds nothing" while this is set.
    pub balances_stale: bool,
    /// Whether the curator has direct exchange quoting enabled (SO-372):
    /// the exchange_adapter witness is in the vault's quote-adapter set,
    /// replayed from TvQuoteAdapterAdded/Removed events. Null when the
    /// deployment has no exchange_adapter package or the replay failed.
    pub direct_quoting_enabled: Option<bool>,
}

pub async fn list_trading_vaults(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TradingVaultsResponse>, StatusCode> {
    let vaults = state.indexer.trading_vaults().await.map_err(|e| {
        tracing::warn!(error = %e, "indexer trading_vaults query failed");
        StatusCode::BAD_GATEWAY
    })?;
    Ok(Json(TradingVaultsResponse {
        vaults: vaults
            .iter()
            .map(|v| trading_vault_dto(&state, v))
            .collect(),
    }))
}

pub async fn get_trading_vault(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<TradingVaultDetailResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The indexer serves the full list only (a handful of vaults); pick ours.
    let vault = state
        .indexer
        .trading_vaults()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading_vaults query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .find(|v| v.vault_id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let positions = state
        .indexer
        .trading_vault_positions(id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading_vault_positions query failed");
            StatusCode::BAD_GATEWAY
        })?;
    // Free balances are a live object read (SO-313): no event states them, so
    // the indexer can't. Degrade to `balances_stale` rather than a 5xx, the
    // same way `GET /vaults/:id` degrades its live round state.
    let balances = match sui_rpc::fetch_vault_balances(&state.http, &state.sui_graphql_url, &id)
        .await
    {
        Ok(Some(b)) => Some(b),
        // An unknown object means the vault the indexer knows is gone from the
        // node's view — report unknown, not "holds nothing".
        Ok(None) => {
            tracing::warn!(vault = %vault_id, "vault object unknown to the RPC; balances omitted");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, vault = %vault_id, "vault balance read failed");
            None
        }
    };
    // SO-372: "direct quoting enabled" = the exchange_adapter's witness is
    // currently in the vault's quote-adapter set. The set isn't materialised
    // (same posture as the SO-370 deposit-asset allowlist), so replay the
    // add/remove events like the other JIT reads in this handler.
    let direct_quoting_enabled = match &state.exchange_adapter_package {
        Some(pkg) => {
            let witness = protocol_types::asset::AssetType::new(format!(
                "{pkg}::exchange_adapter::ExchangeAdapter"
            ))
            .to_canonical();
            match state
                .indexer
                .recent_events_with_payload(
                    &["TvQuoteAdapterAdded", "TvQuoteAdapterRemoved"],
                    json!({ "vault_id": id.to_hex() }),
                    EVENT_SCAN_CAP,
                )
                .await
            {
                Ok(events) => Some(quote_adapter_enabled(events, &witness)),
                Err(e) => {
                    tracing::warn!(error = %e, "indexer quote-adapter events query failed");
                    None
                }
            }
        }
        None => None,
    };
    let balances_stale = balances.is_none();
    let balances = balances
        .unwrap_or_default()
        .into_iter()
        .map(|b| {
            let meta = state.catalog.lookup(&b.coin_type);
            TradingVaultBalanceDto {
                symbol: meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| b.coin_type.clone()),
                decimals: meta.map(|m| m.decimals),
                amount_raw: b.amount.to_string(),
                coin_type: b.coin_type,
            }
        })
        .collect();
    Ok(Json(TradingVaultDetailResponse {
        vault: trading_vault_dto(&state, &vault),
        balances,
        balances_stale,
        direct_quoting_enabled,
        positions: positions
            .into_iter()
            .map(|p| TradingVaultPositionDto {
                position_id: p.position_id.to_hex(),
                adapter: p.adapter.to_canonical(),
                active: p.active,
                stored_at_ms: p.stored_at_ms as i64,
                removed_at_ms: p.removed_at_ms.map(|v| v as i64),
                last_value_raw: p.last_value.map(|v| v.to_string()),
                last_appraised_at_ms: p.last_appraised_at_ms.map(|v| v as i64),
            })
            .collect(),
    }))
}

// ── event-derived analytics (SO-293) ──────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PpsPointDto {
    /// Event time (ms since epoch), decimal string.
    pub timestamp_ms: String,
    /// 1e12-scaled accounting-asset-per-share, decimal string.
    pub pps_e12: String,
    /// `deposit` | `fulfillment` | `appraisal`.
    pub source: String,
}

#[derive(Serialize)]
pub struct PpsHistoryResponse {
    pub points: Vec<PpsPointDto>,
}

/// `GET /trading-vaults/:id/pps-history` — observed pps points, ascending by
/// time. Each TvDeposited implies pps = value/shares (value, not amount —
/// the deposit may be a non-accounting asset, SO-370); each
/// TvWithdrawFulfilled implies pps = value/shares (zero-share / zero-value
/// events carry no price and are skipped). Both are SHARE_OFFSET-rescaled.
/// Each TvVaultAppraised implies
/// pps = total_value/supply, where supply is replayed from the
/// `total_shares` snapshots on the deposit/fulfillment events — without
/// these the curve has one point per flow and a vault that only trades
/// (the desk) never charts at all.
pub async fn get_pps_history(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<PpsHistoryResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvDeposited", "TvWithdrawFulfilled", "TvVaultAppraised"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    Ok(Json(PpsHistoryResponse {
        points: pps_points(events),
    }))
}

fn pps_points(mut events: Vec<protocol_types::events::IndexedEvent>) -> Vec<PpsPointDto> {
    // The scan serves newest-first; both the supply replay and the response
    // contract need chain order.
    events.sort_by_key(|e| e.sequence);

    let mut points = Vec::new();
    let mut supply: u128 = 0;
    for ev in &events {
        let (pps_e12, source) = match &ev.event {
            ChainEvent::TvDeposited(d) => {
                supply = d.total_shares;
                if d.shares == 0 {
                    continue;
                }
                (
                    d.value as u128 * PPS_E12 * SHARE_OFFSET / d.shares,
                    "deposit",
                )
            }
            ChainEvent::TvWithdrawFulfilled(f) => {
                supply = f.total_shares;
                if f.shares == 0 || f.value == 0 {
                    continue;
                }
                (
                    f.value as u128 * PPS_E12 * SHARE_OFFSET / f.shares,
                    "fulfillment",
                )
            }
            // An appraisal consumed by a deposit/fulfillment is emitted
            // before that event's mint/burn, so the pre-event supply is the
            // right divisor for its NAV.
            ChainEvent::TvVaultAppraised(a) if supply != 0 => {
                (a.total_value * PPS_E12 * SHARE_OFFSET / supply, "appraisal")
            }
            _ => continue,
        };
        points.push(PpsPointDto {
            timestamp_ms: ev.timestamp_ms.to_string(),
            pps_e12: pps_e12.to_string(),
            source: source.to_string(),
        });
    }
    points
}

/// One curator spot trade — a `deepbook_adapter` taker swap of vault free
/// balances against an allowlisted pool (SO-313).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakerSwapDto {
    /// Event time (ms since epoch), decimal string.
    pub timestamp_ms: String,
    /// Digest of the transaction that executed the swap.
    pub tx_digest: String,
    pub pool_id: String,
    /// True when the vault sold the pool's base asset for its quote asset.
    /// The pool's type args say which coin types those are; the event carries
    /// only the direction.
    pub base_for_quote: bool,
    /// Raw u64 amounts in the respective assets' atomic units, decimal
    /// strings.
    pub amount_in: String,
    pub amount_out: String,
    /// Input returned unfilled (lot rounding or a thin book).
    pub unswapped: String,
}

#[derive(Serialize)]
pub struct TakerSwapsResponse {
    pub trades: Vec<TakerSwapDto>,
}

/// `GET /trading-vaults/:id/trades` — the vault's curator spot trades, most
/// recent first.
///
/// Read from the event log rather than a materialised view: a taker swap
/// moves value between the vault's free balances and creates no entity to
/// materialise (`deepbook_adapter.move` never calls `vault::put_position`),
/// so the indexer's `TvTakerSwapExecuted` view arms are correctly no-ops.
/// Same shape as `/pps-history` above.
pub async fn get_trades(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<TakerSwapsResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let events = state
        .indexer
        .recent_events_with_payload_and_tx(
            &["TvTakerSwapExecuted"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer taker-swap events query failed");
            StatusCode::BAD_GATEWAY
        })?;

    let mut trades: Vec<TakerSwapDto> = events
        .iter()
        .filter_map(|ev| match &ev.event.event {
            ChainEvent::TvTakerSwapExecuted(s) => Some(TakerSwapDto {
                timestamp_ms: ev.event.timestamp_ms.to_string(),
                tx_digest: ev.tx_digest.clone(),
                pool_id: s.pool_id.to_hex(),
                base_for_quote: s.base_for_quote,
                amount_in: s.amount_in.to_string(),
                amount_out: s.amount_out.to_string(),
                unswapped: s.unswapped.to_string(),
            }),
            _ => None,
        })
        .collect();
    // The client pages ascending by sequence; a trade list reads newest-first.
    trades.reverse();
    Ok(Json(TakerSwapsResponse { trades }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StakeResponse {
    /// Live share balance, decimal string (u128).
    pub shares: String,
    /// Accounting-asset cost basis of the live shares, decimal string (u64).
    pub cost_basis: String,
    /// shares × latest observed pps / 1e12; null when the vault has no
    /// observed pps yet.
    pub estimated_value: Option<String>,
    /// Lockup expiry from the wallet's most recent deposit; null if the
    /// wallet never deposited.
    pub locked_until_ms: Option<String>,
}

/// `GET /trading-vaults/:id/stake/:address` — one wallet's live stake,
/// replayed from the vault's deposit / withdraw-request events. Curator
/// cap-keyed stakes (`curator_cap != null`) are out of scope — address
/// stakes only.
pub async fn get_stake(
    State(state): State<Arc<AppState>>,
    Path((vault_id, address)): Path<(String, String)>,
) -> Result<Json<StakeResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let addr = SuiAddress::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The indexer serves the full list only (a handful of vaults); pick ours.
    let vault = state
        .indexer
        .trading_vaults()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading_vaults query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .find(|v| v.vault_id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvDeposited", "TvWithdrawRequested"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;

    let mut shares: u128 = 0;
    let mut cost_basis: u64 = 0;
    let mut locked_until_ms: Option<u64> = None;
    for ev in &events {
        match &ev.event {
            ChainEvent::TvDeposited(d) if d.depositor == addr && d.curator_cap.is_none() => {
                shares = shares.saturating_add(d.shares);
                // `value`, not `amount`: the deposit may be a non-accounting
                // asset (SO-370); basis is tracked in accounting units.
                cost_basis = cost_basis.saturating_add(d.value);
                locked_until_ms = Some(d.locked_until_ms);
            }
            ChainEvent::TvWithdrawRequested(w)
                if w.recipient == addr && w.curator_cap.is_none() =>
            {
                shares = shares.saturating_sub(w.shares);
                cost_basis = cost_basis.saturating_sub(w.basis);
            }
            _ => {}
        }
    }
    // shares × pps can't realistically overflow u128 (shares ≲ 1e26,
    // pps ≲ 1e15), but degrade to null rather than a wrong number if it does.
    // pps is SHARE_OFFSET-rescaled (shares carry the 1e6 virtual offset), so
    // the divisor carries it too.
    let estimated_value = vault
        .latest_pps_e12
        .and_then(|pps| shares.checked_mul(pps))
        .map(|v| (v / (PPS_E12 * SHARE_OFFSET)).to_string());
    Ok(Json(StakeResponse {
        shares: shares.to_string(),
        cost_basis: cost_basis.to_string(),
        estimated_value,
        locked_until_ms: locked_until_ms.map(|v| v.to_string()),
    }))
}

/// One not-yet-fulfilled withdraw request, replayed from the vault's
/// request / amend / fulfil events (SO-370).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequestDto {
    /// The request's queue sequence number, decimal string.
    pub seq: String,
    pub recipient: String,
    /// Escrowed shares, decimal string (u128).
    pub shares_raw: String,
    /// Accounting-asset cost basis, decimal string (u64).
    pub basis_raw: String,
    /// Canonical coin type the recipient asked to be paid in (latest
    /// PayoutAssetAmended wins).
    pub payout_coin_type: String,
    /// Catalog symbol, falling back to the coin type when unknown.
    pub payout_symbol: String,
    pub requested_at_ms: String,
}

#[derive(Serialize)]
pub struct PendingRequestsResponse {
    pub requests: Vec<PendingRequestDto>,
}

/// `GET /trading-vaults/:id/pending-requests` — the vault's outstanding
/// withdraw queue with each request's payout asset, ascending by seq.
/// Replayed from the event log like `/pps-history`: requests have no
/// materialised view, and a pending request's payout asset can change via
/// TvPayoutAssetAmended.
pub async fn get_pending_requests(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<PendingRequestsResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &[
                "TvWithdrawRequested",
                "TvPayoutAssetAmended",
                "TvWithdrawFulfilled",
            ],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let requests = pending_requests(events)
        .into_iter()
        .map(|r| {
            let coin_type = r.payout_asset.to_canonical();
            let meta = state.catalog.lookup(r.payout_asset.as_str());
            PendingRequestDto {
                seq: r.seq.to_string(),
                recipient: r.recipient.to_hex(),
                shares_raw: r.shares.to_string(),
                basis_raw: r.basis.to_string(),
                payout_symbol: meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| coin_type.clone()),
                payout_coin_type: coin_type,
                requested_at_ms: r.requested_at_ms.to_string(),
            }
        })
        .collect();
    Ok(Json(PendingRequestsResponse { requests }))
}

struct PendingRequest {
    seq: u64,
    recipient: SuiAddress,
    shares: u128,
    basis: u64,
    payout_asset: protocol_types::asset::AssetType,
    requested_at_ms: u64,
}

/// Replay the withdraw queue: a request enters at TvWithdrawRequested, its
/// payout asset follows the latest TvPayoutAssetAmended, and it leaves at
/// TvWithdrawFulfilled. Returns the survivors, ascending by seq.
fn pending_requests(mut events: Vec<protocol_types::events::IndexedEvent>) -> Vec<PendingRequest> {
    // The scan serves newest-first; the replay needs chain order.
    events.sort_by_key(|e| e.sequence);
    let mut open: std::collections::BTreeMap<u64, PendingRequest> =
        std::collections::BTreeMap::new();
    for ev in events {
        match ev.event {
            ChainEvent::TvWithdrawRequested(w) => {
                open.insert(
                    w.seq,
                    PendingRequest {
                        seq: w.seq,
                        recipient: w.recipient,
                        shares: w.shares,
                        basis: w.basis,
                        payout_asset: w.payout_asset,
                        requested_at_ms: w.requested_at_ms,
                    },
                );
            }
            ChainEvent::TvPayoutAssetAmended(a) => {
                if let Some(r) = open.get_mut(&a.seq) {
                    r.payout_asset = a.payout_asset;
                }
            }
            ChainEvent::TvWithdrawFulfilled(f) => {
                open.remove(&f.seq);
            }
            _ => {}
        }
    }
    open.into_values().collect()
}

/// Replay the quote-adapter set for one witness: the latest add/remove wins.
/// `witness` is the canonical type string of the adapter.
fn quote_adapter_enabled(
    mut events: Vec<protocol_types::events::IndexedEvent>,
    witness: &str,
) -> bool {
    // The scan serves newest-first; the replay needs chain order.
    events.sort_by_key(|e| e.sequence);
    let mut enabled = false;
    for ev in events {
        match ev.event {
            ChainEvent::TvQuoteAdapterAdded(a) if a.adapter.to_canonical() == witness => {
                enabled = true;
            }
            ChainEvent::TvQuoteAdapterRemoved(r) if r.adapter.to_canonical() == witness => {
                enabled = false;
            }
            _ => {}
        }
    }
    enabled
}

fn trading_vault_dto(state: &AppState, v: &TradingVault) -> TradingVaultDto {
    let meta = state.catalog.lookup(v.accounting_asset.as_str());
    TradingVaultDto {
        vault_id: v.vault_id.to_hex(),
        accounting_symbol: meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| v.accounting_asset.as_str().to_string()),
        accounting_decimals: meta.map(|m| m.decimals),
        accounting_coin_type: v.accounting_asset.to_canonical(),
        creator: v.creator.to_hex(),
        curator: v.curator.to_hex(),
        curator_cap_id: v.curator_cap_id.to_hex(),
        state: v.state.clone(),
        lockup_ms: v.lockup_ms as i64,
        curator_fee_bps: v.curator_fee_bps as i64,
        unwind_grace_ms: v.unwind_grace_ms as i64,
        deposits_paused: v.deposits_paused,
        mm_release_enabled: v.mm_release_enabled,
        total_shares_raw: v.total_shares.to_string(),
        position_count: v.position_count as i64,
        pending_withdrawals: v.pending_withdrawals as i64,
        pps: v.latest_pps_e12.map(|p| p as f64 / PPS_SCALE),
        pps_raw: v.latest_pps_e12.map(|p| p.to_string()),
        updated_at_ms: v.updated_at_ms as i64,
        external_account: v.external_account.map(|a| a.to_hex()),
        external_exposure: v.external_exposure.to_string(),
        latest_external_equity: v.latest_external_equity.map(|e| e.to_string()),
        external_equity_updated_at_ms: v.external_equity_updated_at_ms.map(|t| t.to_string()),
        latest_nav_raw: v.latest_nav.map(|n| n.to_string()),
        nav_updated_at_ms: v.nav_updated_at_ms.map(|t| t as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::events::{
        IndexedEvent, TvDeposited, TvVaultAppraised, TvWithdrawFulfilled,
    };

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_hex(&format!("0x{:064x}", n)).unwrap()
    }

    fn ev(sequence: u64, event: ChainEvent) -> IndexedEvent {
        IndexedEvent {
            sequence,
            timestamp_ms: 1_000 + sequence,
            event,
        }
    }

    /// Newest-first input (the scan's order) must come back as an ascending
    /// curve, with appraisals priced against the replayed share supply.
    /// Shares carry the Move-side SHARE_OFFSET (×1e6): a genesis deposit of
    /// V accounting units mints V×1e6 shares, and pps still reads 1e12.
    #[test]
    fn pps_points_replays_supply_across_event_kinds() {
        let vault_id = oid(1);
        let depositor = SuiAddress::from_hex(&format!("0x{:064x}", 2)).unwrap();
        let tusdc = protocol_types::asset::AssetType::new("9b::tusdc::TUSDC");
        let deposit = ChainEvent::TvDeposited(TvDeposited {
            vault_id,
            depositor,
            curator_cap: None,
            asset: tusdc.clone(),
            amount: 1_000_000,
            value: 1_000_000,
            shares: 1_000_000 * SHARE_OFFSET,
            total_shares: 1_000_000 * SHARE_OFFSET,
            locked_until_ms: 0,
        });
        // NAV grew 2% with no flows: pps only observable via the appraisal.
        let appraised = ChainEvent::TvVaultAppraised(TvVaultAppraised {
            vault_id,
            total_value: 1_020_000,
            position_total: 0,
        });
        let fulfilled = ChainEvent::TvWithdrawFulfilled(TvWithdrawFulfilled {
            vault_id,
            seq: 0,
            recipient: depositor,
            shares: 500_000 * SHARE_OFFSET,
            value: 510_000,
            basis: 500_000,
            profit: 10_000,
            gross_fee: 0,
            protocol_cut: 0,
            curator_net: 0,
            curator_shares_minted: 0,
            payout: 510_000,
            payout_asset: tusdc,
            payout_units: 510_000,
            price: PPS_E12,
            total_shares: 500_000 * SHARE_OFFSET,
        });
        // Pre-supply appraisal (sequence 0) carries no price and is dropped.
        let orphan = ChainEvent::TvVaultAppraised(TvVaultAppraised {
            vault_id,
            total_value: 999,
            position_total: 0,
        });

        let newest_first = vec![
            ev(3, fulfilled),
            ev(2, appraised),
            ev(1, deposit),
            ev(0, orphan),
        ];
        let points = pps_points(newest_first);

        let got: Vec<(&str, &str)> = points
            .iter()
            .map(|p| (p.source.as_str(), p.pps_e12.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("deposit", "1000000000000"),
                ("appraisal", "1020000000000"),
                ("fulfillment", "1020000000000"),
            ],
        );
        let times: Vec<&str> = points.iter().map(|p| p.timestamp_ms.as_str()).collect();
        assert_eq!(times, vec!["1001", "1002", "1003"]);
    }

    /// The queue replay keeps unfulfilled requests, applies the latest
    /// payout-asset amendment, and drops fulfilled seqs (SO-370).
    #[test]
    fn pending_requests_replays_the_withdraw_queue() {
        use protocol_types::asset::AssetType;
        use protocol_types::events::{TvPayoutAssetAmended, TvWithdrawRequested};

        let vault_id = oid(1);
        let recipient = SuiAddress::from_hex(&format!("0x{:064x}", 2)).unwrap();
        let tusdc = AssetType::new("9b::tusdc::TUSDC");
        let tbtc = AssetType::new("9b::tbtc::TBTC");
        let request = |seq: u64, at: u64| {
            ChainEvent::TvWithdrawRequested(TvWithdrawRequested {
                vault_id,
                seq,
                recipient,
                curator_cap: None,
                shares: 100 * SHARE_OFFSET,
                basis: 100,
                payout_asset: tusdc.clone(),
                requested_at_ms: at,
            })
        };
        let amend = ChainEvent::TvPayoutAssetAmended(TvPayoutAssetAmended {
            vault_id,
            seq: 1,
            payout_asset: tbtc.clone(),
        });
        let fulfilled = ChainEvent::TvWithdrawFulfilled(TvWithdrawFulfilled {
            vault_id,
            seq: 0,
            recipient,
            shares: 100 * SHARE_OFFSET,
            value: 100,
            basis: 100,
            profit: 0,
            gross_fee: 0,
            protocol_cut: 0,
            curator_net: 0,
            curator_shares_minted: 0,
            payout: 100,
            payout_asset: tusdc.clone(),
            payout_units: 100,
            price: PPS_E12,
            total_shares: 100 * SHARE_OFFSET,
        });

        // Newest-first input, as the scan serves it.
        let events = vec![
            ev(3, fulfilled),
            ev(2, amend),
            ev(1, request(1, 2_000)),
            ev(0, request(0, 1_000)),
        ];
        let pending = pending_requests(events);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].seq, 1);
        assert_eq!(pending[0].payout_asset, tbtc);
        assert_eq!(pending[0].requested_at_ms, 2_000);
    }
}
