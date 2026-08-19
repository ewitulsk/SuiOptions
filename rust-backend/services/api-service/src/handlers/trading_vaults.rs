//! Curated trading vault endpoints (SO-287):
//!
//!   - `GET /trading-vaults`     — list with headline state / observed pps
//!   - `GET /trading-vaults/:id` — one vault + its adapter positions (past
//!     positions included, `active=false`)
//!
//! Event-derived analytics (SO-293, reshaped for trading-vault v2 in
//! SO-418):
//!
//!   - `GET /trading-vaults/:id/pps-history`     — per-tranche observed pps
//!     points from TvDeposited / TvWithdrawFulfilled / TvCapitalSynced
//!     events, ascending by time
//!   - `GET /trading-vaults/:id/trades`          — curator spot trades from
//!     TvTakerSwapExecuted events, newest first (SO-313)
//!   - `GET /trading-vaults/:id/pending-requests` — outstanding withdraw
//!     queue with lane / position / payability fields (SO-370, SO-418)
//!   - `GET /trading-vaults/:id/waterfall`       — the §3.4a decomposition
//!     at the latest capital sync (SO-418)
//!   - `GET /trading-vaults/:id/settlement`      — terminal settlement pool
//!     status (SO-418)
//!
//! Position NFT reads (SO-418; replaces `stake/:address` — v2 stakes are
//! transferable `VaultPosition` objects, so ownership is a live chain fact,
//! not an event-derived one):
//!
//!   - `GET /trading-vaults/:id/positions/:address` — a wallet's live
//!     positions (owned-object query by type, filtered to the vault)
//!   - `GET /trading-vaults/positions/:positionId`  — one position by id,
//!     any holder, with its current owner
//!
//! All reads are JIT GraphQL queries to the indexer, except the detail
//! endpoint's `balances[]` and the position reads — those are live Sui
//! reads (`sui_rpc`). Balances degrade to `balances_stale` on failure.

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
/// earliest queue entries), not the newest.
const EVENT_SCAN_CAP: usize = 5000;

/// Basis-point denominator for the §3.4a waterfall / fee math.
const BPS: u128 = 10_000;

/// Wire-code labels (OFFCHAIN_BRIEF frozen codes). Unknown codes label as
/// `unknown` rather than failing a whole response.
fn tranche_label(code: u8) -> &'static str {
    match code {
        0 => "untranched",
        1 => "senior",
        2 => "junior",
        _ => "unknown",
    }
}

fn risk_state_label(code: u8) -> &'static str {
    match code {
        0 => "healthy",
        1 => "coverage_breach",
        2 => "impaired",
        3 => "reset_pending",
        _ => "unknown",
    }
}

fn lane_label(code: u8) -> &'static str {
    match code {
        0 => "senior",
        _ => "junior",
    }
}

fn upside_label(code: u8) -> &'static str {
    match code {
        0 => "preferred_only",
        1 => "capped_participating",
        2 => "uncapped_participating",
        _ => "unknown",
    }
}

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
    // ── trading-vault v2 capital structure (SO-418) ──
    /// Null for untranched vaults.
    pub capital_structure: Option<CapitalStructureDto>,
    pub terms_version: i64,
    /// 0x-hex spec hash; null when absent.
    pub spec_hash: Option<String>,
    /// healthy | coverage_breach | impaired | reset_pending.
    pub risk_state: String,
    pub risk_state_code: u8,
    pub curator_commitment_breached: bool,
    pub senior_shares_raw: String,
    /// Untranched supply lives here (mirrors capital.move).
    pub junior_shares_raw: String,
    pub senior_claim_raw: String,
    /// Waterfall NAV split from the latest TvCapitalSynced; null before
    /// the first sync.
    pub senior_nav_raw: Option<String>,
    pub junior_nav_raw: Option<String>,
    /// Per-tranche observed pps (PPS_SCALE-adjusted float + raw string).
    pub senior_pps: Option<f64>,
    pub senior_pps_raw: Option<String>,
    pub junior_pps: Option<f64>,
    pub junior_pps_raw: Option<String>,
    /// junior_nav × 1e4 / nav from the latest sync; null before it.
    pub junior_buffer_bps: Option<i64>,
    pub impaired_since_ms: Option<i64>,
    pub active_junior_generation: i64,
    /// Open junior-reset proposal; null when none.
    pub reset_proposal: Option<ResetProposalDto>,
    /// Terminal settlement snapshot taken.
    pub settled: bool,
    pub lane_heads: LaneHeadsDto,
}

/// Immutable senior/junior terms (contract `capital_structure`).
#[derive(Serialize)]
pub struct CapitalStructureDto {
    pub senior_hurdle_bps_annual: i64,
    pub target_junior_bps: i64,
    pub maintenance_junior_bps: i64,
    /// preferred_only | capped_participating | uncapped_participating.
    pub upside: String,
    pub residual_participation_bps: i64,
    pub total_return_cap_bps: i64,
}

/// Open junior generational reset proposal (disclosure terms).
#[derive(Serialize)]
pub struct ResetProposalDto {
    pub old_generation: i64,
    pub proposed_at_ms: i64,
    pub executable_at_ms: i64,
    pub recorded_nav_raw: String,
    pub recorded_senior_claim_raw: String,
    pub recorded_required_deposit_raw: String,
}

/// Per-lane FIFO cursors: tail = highest requested global_seq + 1, head =
/// highest fulfilled/settled global_seq + 1.
#[derive(Serialize)]
pub struct LaneCursorDto {
    pub head: i64,
    pub tail: i64,
}

#[derive(Serialize)]
pub struct LaneHeadsDto {
    pub senior: LaneCursorDto,
    pub junior: LaneCursorDto,
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
pub struct PpsPointDto {
    /// Event time (ms since epoch).
    pub timestamp_ms: i64,
    /// untranched | senior | junior.
    pub tranche: String,
    /// PPS_SCALE-adjusted float + 1e12-scaled raw decimal string.
    pub pps: f64,
    pub pps_raw: String,
    /// `deposit` | `withdraw` | `capital_sync`.
    pub source: String,
    /// True on the first junior point of a new generation (junior PPS
    /// re-bases to 1.0 at reset execution).
    pub reset: bool,
}

#[derive(Serialize)]
pub struct PpsHistoryResponse {
    pub points: Vec<PpsPointDto>,
}

/// `GET /trading-vaults/:id/pps-history` — per-tranche observed pps points,
/// ascending by time (SO-418).
///
/// Each TvDeposited / TvWithdrawFulfilled implies its tranche's
/// pps = value/shares (value, not amount — the deposit may be a
/// non-accounting asset, SO-370; zero-share / zero-value events carry no
/// price and are skipped), SHARE_OFFSET-rescaled. Each TvCapitalSynced
/// implies each populated tranche's claim ratio
/// `(nav_t + 1) / (S_t + OFFSET)` — this replaces the v1 TvVaultAppraised
/// supply replay: every consumed appraisal now emits a capital sync, so a
/// vault that only trades still charts.
pub async fn get_pps_history(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<PpsHistoryResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The vault's structure decides whether sync points label as
    // `untranched` or as per-tranche series.
    let vault = find_vault(&state, id).await?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvDeposited", "TvWithdrawFulfilled", "TvCapitalSynced"],
            json!({ "vault_id": id.to_hex() }),
            EVENT_SCAN_CAP,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading-vault events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    Ok(Json(PpsHistoryResponse {
        points: pps_points(events, vault.structure_code),
    }))
}

/// The tranche claim ratio as an e12 pps: `(nav + 1) × 1e12 × OFFSET /
/// (shares + OFFSET)` — exactly the §3.3 pricing both mints and claims use,
/// so sync points land on the same curve as flow points. None on overflow
/// (degrade to a gap, never a wrong number).
fn claim_ratio_e12(nav: u128, shares: u128) -> Option<u128> {
    (nav.checked_add(1)?)
        .checked_mul(PPS_E12)?
        .checked_mul(SHARE_OFFSET)
        .map(|n| n / (shares + SHARE_OFFSET))
}

fn pps_points(
    mut events: Vec<protocol_types::events::IndexedEvent>,
    structure_code: u8,
) -> Vec<PpsPointDto> {
    // The scan serves newest-first; the response contract needs chain order.
    events.sort_by_key(|e| e.sequence);
    let untranched = structure_code == 0;

    let mut points = Vec::new();
    // Junior generation watermark: a junior point under a HIGHER generation
    // than the last junior point marks the re-base (`reset: true`).
    let mut last_junior_gen: Option<u64> = None;
    let mut push = |ts: i64, tranche: u8, pps_e12: u128, source: &str, generation: u64| {
        let reset = tranche == 2 && last_junior_gen.is_some_and(|g| generation > g);
        if tranche == 2 {
            last_junior_gen = Some(generation);
        }
        points.push(PpsPointDto {
            timestamp_ms: ts,
            tranche: tranche_label(tranche).to_string(),
            pps: pps_e12 as f64 / PPS_SCALE,
            pps_raw: pps_e12.to_string(),
            source: source.to_string(),
            reset,
        });
    };
    for ev in &events {
        let ts = ev.timestamp_ms as i64;
        match &ev.event {
            ChainEvent::TvDeposited(d) => {
                if d.shares == 0 {
                    continue;
                }
                let Some(pps) = (d.value as u128)
                    .checked_mul(PPS_E12 * SHARE_OFFSET)
                    .map(|n| n / d.shares)
                else {
                    continue;
                };
                push(ts, d.tranche, pps, "deposit", d.capital_generation);
            }
            ChainEvent::TvWithdrawFulfilled(f) => {
                if f.shares == 0 || f.value == 0 {
                    continue;
                }
                let Some(pps) = (f.value as u128)
                    .checked_mul(PPS_E12 * SHARE_OFFSET)
                    .map(|n| n / f.shares)
                else {
                    continue;
                };
                push(ts, f.tranche, pps, "withdraw", f.capital_generation);
            }
            ChainEvent::TvCapitalSynced(c) => {
                if untranched {
                    // The single untranched book lives in the junior fields;
                    // its NAV is the whole NAV.
                    if c.junior_shares > 0 {
                        if let Some(pps) = claim_ratio_e12(c.total_nav, c.junior_shares) {
                            push(ts, 0, pps, "capital_sync", 0);
                        }
                    }
                    continue;
                }
                if c.senior_shares > 0 {
                    if let Some(pps) = claim_ratio_e12(c.senior_nav, c.senior_shares) {
                        push(ts, 1, pps, "capital_sync", 0);
                    }
                }
                if c.junior_shares > 0 {
                    if let Some(pps) = claim_ratio_e12(c.junior_nav, c.junior_shares) {
                        push(ts, 2, pps, "capital_sync", c.active_junior_generation);
                    }
                }
            }
            _ => continue,
        }
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

// ── VaultPosition NFT reads (SO-418; replaces stake/:address) ─────────────

/// One live `VaultPosition` NFT (contract shape).
#[derive(Serialize)]
pub struct VaultPositionDto {
    pub position_id: String,
    pub vault_id: String,
    /// untranched | senior | junior.
    pub tranche: String,
    pub tranche_code: u8,
    pub capital_generation: i64,
    /// Junior position of a retired generation — permanently zero-value.
    pub wiped: bool,
    pub shares_raw: String,
    pub cost_basis_raw: String,
    pub locked_until_ms: i64,
    /// shares × (nav_t + 1) / (S_t + OFFSET) at the latest capital sync;
    /// null before the first sync. Wiped positions estimate 0.
    pub estimated_value_raw: Option<String>,
    /// max(value − basis, 0).
    pub estimated_profit_raw: Option<String>,
    /// profit × curator_fee_bps / 1e4 — the embedded fee liability an exit
    /// would crystallize today.
    pub estimated_fee_raw: Option<String>,
}

#[derive(Serialize)]
pub struct VaultPositionsResponse {
    pub positions: Vec<VaultPositionDto>,
}

#[derive(Serialize)]
pub struct VaultPositionDetailResponse {
    #[serde(flatten)]
    pub position: VaultPositionDto,
    /// Current owner address; null when the holder isn't a plain address
    /// (shared / object-owned / kiosk'd).
    pub owner: Option<String>,
}

/// The indexer serves the full vault list only (a handful of vaults); pick
/// ours or 404.
async fn find_vault(state: &AppState, id: ObjectId) -> Result<TradingVault, StatusCode> {
    state
        .indexer
        .trading_vaults()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer trading_vaults query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .find(|v| v.vault_id == id)
        .ok_or(StatusCode::NOT_FOUND)
}

/// The `{pkg}::vault_position::VaultPosition` type the live reads query by;
/// 503 when the deployment has no trading-vault package.
fn position_type(state: &AppState) -> Result<String, StatusCode> {
    let pkg = state.trading_vault_package.as_deref().ok_or_else(|| {
        tracing::warn!("position read refused: no trading_vault package in the token-info snapshot");
        StatusCode::SERVICE_UNAVAILABLE
    })?;
    Ok(format!("{pkg}::vault_position::VaultPosition"))
}

/// Build the contract DTO for one live position, enriched with estimates
/// from the vault's latest tranche ratio.
fn vault_position_dto(v: &TradingVault, p: &sui_rpc::VaultPositionLive) -> VaultPositionDto {
    let wiped = p.tranche == 2 && p.capital_generation < v.active_junior_generation;
    let (value, profit, fee) = if wiped {
        // A wiped generation is permanently zero-value (§8.5).
        (Some(0), Some(0), Some(0))
    } else {
        // Senior prices off the senior book; junior AND untranched off the
        // junior fields (the untranched book lives there, and the waterfall
        // gives untranched vaults junior_nav == NAV).
        let (nav_t, supply, observed_pps) = match p.tranche {
            1 => (v.senior_nav, v.senior_shares, v.latest_senior_pps_e12),
            2 => (v.junior_nav, v.junior_shares, v.latest_junior_pps_e12),
            _ => (v.junior_nav, v.junior_shares, v.latest_pps_e12),
        };
        position_estimates(
            nav_t,
            supply,
            observed_pps,
            v.curator_fee_bps,
            p.shares,
            p.cost_basis,
        )
    };
    VaultPositionDto {
        position_id: p.position_id.to_hex(),
        vault_id: p.vault_id.to_hex(),
        tranche: tranche_label(p.tranche).to_string(),
        tranche_code: p.tranche,
        capital_generation: p.capital_generation as i64,
        wiped,
        shares_raw: p.shares.to_string(),
        cost_basis_raw: p.cost_basis.to_string(),
        locked_until_ms: p.locked_until_ms as i64,
        estimated_value_raw: value.map(|x: u128| x.to_string()),
        estimated_profit_raw: profit.map(|x: u128| x.to_string()),
        estimated_fee_raw: fee.map(|x: u128| x.to_string()),
    }
}

/// Estimate (value, profit, fee) for a live position from its tranche's
/// latest ratio: `value = shares × (nav_t + 1) / (S_t + OFFSET)` where
/// `nav_t`/`S_t` come from the latest TvCapitalSynced (indexer). Before the
/// first sync, falls back to the tranche's observed pps; all-null when
/// neither exists (or on overflow — degrade, never a wrong number).
fn position_estimates(
    nav_t: Option<u128>,
    supply: u128,
    observed_pps: Option<u128>,
    curator_fee_bps: u64,
    shares: u128,
    basis: u64,
) -> (Option<u128>, Option<u128>, Option<u128>) {
    let value = match nav_t {
        Some(nav) => shares
            .checked_mul(nav.saturating_add(1))
            .map(|n| n / (supply + SHARE_OFFSET)),
        None => observed_pps
            .and_then(|pps| shares.checked_mul(pps))
            .map(|n| n / (PPS_E12 * SHARE_OFFSET)),
    };
    let Some(value) = value else {
        return (None, None, None);
    };
    let profit = value.saturating_sub(basis as u128);
    let fee = profit.saturating_mul(curator_fee_bps as u128) / BPS;
    (Some(value), Some(profit), Some(fee))
}

/// `GET /trading-vaults/:id/positions/:address` — the wallet's live
/// `VaultPosition` NFTs in this vault, straight off chain (transfers emit
/// no events, so ownership can't be indexed).
pub async fn get_wallet_positions(
    State(state): State<Arc<AppState>>,
    Path((vault_id, address)): Path<(String, String)>,
) -> Result<Json<VaultPositionsResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let addr = SuiAddress::from_hex(&address).map_err(|_| StatusCode::BAD_REQUEST)?;
    let ty = position_type(&state)?;
    let vault = find_vault(&state, id).await?;
    let owned =
        sui_rpc::fetch_owned_vault_positions(&state.http, &state.sui_graphql_url, &addr, &ty)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, wallet = %address, "owned vault-positions read failed");
                StatusCode::BAD_GATEWAY
            })?;
    Ok(Json(VaultPositionsResponse {
        positions: owned
            .iter()
            .filter(|p| p.vault_id == id)
            .map(|p| vault_position_dto(&vault, p))
            .collect(),
    }))
}

/// `GET /trading-vaults/positions/:positionId` — one position by id, for
/// ANY holder (positions are transferable; the detail page must render one
/// you just bought). 404 when the object doesn't exist or isn't a
/// `VaultPosition`.
pub async fn get_position(
    State(state): State<Arc<AppState>>,
    Path(position_id): Path<String>,
) -> Result<Json<VaultPositionDetailResponse>, StatusCode> {
    let pid = ObjectId::from_hex(&position_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let ty = position_type(&state)?;
    let (position, owner) =
        sui_rpc::fetch_vault_position(&state.http, &state.sui_graphql_url, &pid, &ty)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, position = %position_id, "vault-position read failed");
                StatusCode::BAD_GATEWAY
            })?
            .ok_or(StatusCode::NOT_FOUND)?;
    let vault = find_vault(&state, position.vault_id).await?;
    Ok(Json(VaultPositionDetailResponse {
        position: vault_position_dto(&vault, &position),
        owner,
    }))
}

/// One not-yet-fulfilled withdraw request, replayed from the vault's
/// request / amend / fulfil / settle events (SO-370, SO-418).
///
/// Serialization note: the pre-v2 fields keep their camelCase wire names;
/// the v2 additions are pinned snake_case by the WS-3 DTO contract, hence
/// the explicit renames.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRequestDto {
    /// The request's queue sequence number, decimal string (the vault-wide
    /// global sequence; `global_seq` aliases it per the v2 contract).
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
    // ── v2 additions (SO-418), snake_case per the WS-3 contract ──
    #[serde(rename = "global_seq")]
    pub global_seq: String,
    /// senior | junior lane label.
    pub lane: String,
    #[serde(rename = "lane_code")]
    pub lane_code: u8,
    /// The consumed `VaultPosition`.
    #[serde(rename = "position_id")]
    pub position_id: String,
    /// untranched | senior | junior.
    pub tranche: String,
    #[serde(rename = "tranche_code")]
    pub tranche_code: u8,
    #[serde(rename = "capital_generation")]
    pub capital_generation: i64,
    /// Whether fulfillment could pay this request right now.
    pub payable: bool,
    #[serde(rename = "blocked_reason")]
    pub blocked_reason: Option<String>,
}

#[derive(Serialize)]
pub struct PendingRequestsResponse {
    pub requests: Vec<PendingRequestDto>,
}

/// `GET /trading-vaults/:id/pending-requests` — the vault's outstanding
/// withdraw queue with each request's payout asset and lane/payability
/// state, ascending by global_seq. Replayed from the event log like
/// `/pps-history`: requests have no materialised view, a pending request's
/// payout asset can change via TvPayoutAssetAmended, and after settlement
/// queued requests drain via TvSettlementRedeemed instead of fulfillment.
pub async fn get_pending_requests(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<PendingRequestsResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    // Payability needs the vault's risk state + active junior generation.
    let vault = find_vault(&state, id).await?;
    let events = state
        .indexer
        .recent_events_with_payload(
            &[
                "TvWithdrawRequested",
                "TvPayoutAssetAmended",
                "TvWithdrawFulfilled",
                "TvSettlementRedeemed",
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
            let (payable, blocked_reason) =
                request_payability(vault.risk_state, vault.active_junior_generation, &r);
            PendingRequestDto {
                seq: r.global_seq.to_string(),
                recipient: r.recipient.to_hex(),
                shares_raw: r.shares.to_string(),
                basis_raw: r.basis.to_string(),
                payout_symbol: meta
                    .map(|m| m.symbol.clone())
                    .unwrap_or_else(|| coin_type.clone()),
                payout_coin_type: coin_type,
                requested_at_ms: r.requested_at_ms.to_string(),
                global_seq: r.global_seq.to_string(),
                lane: lane_label(r.lane).to_string(),
                lane_code: r.lane,
                position_id: r.position_id.to_hex(),
                tranche: tranche_label(r.tranche).to_string(),
                tranche_code: r.tranche,
                capital_generation: r.capital_generation as i64,
                payable,
                blocked_reason: blocked_reason.map(str::to_string),
            }
        })
        .collect();
    Ok(Json(PendingRequestsResponse { requests }))
}

struct PendingRequest {
    global_seq: u64,
    lane: u8,
    position_id: ObjectId,
    recipient: SuiAddress,
    tranche: u8,
    capital_generation: u64,
    shares: u128,
    basis: u64,
    payout_asset: protocol_types::asset::AssetType,
    requested_at_ms: u64,
}

/// Whether fulfillment could pay this request right now (§7 action
/// matrix): a wiped-generation junior request settles at zero
/// (`wiped_generation`), and the junior lane is blocked while the vault is
/// risk-state blocked (`junior_lane_blocked`). The commitment-breach flag
/// does NOT block the junior lane (spec §7).
fn request_payability(
    risk_state: u8,
    active_junior_generation: u64,
    r: &PendingRequest,
) -> (bool, Option<&'static str>) {
    if r.tranche == 2 && r.capital_generation < active_junior_generation {
        return (false, Some("wiped_generation"));
    }
    if r.lane == 1 && risk_state != 0 {
        return (false, Some("junior_lane_blocked"));
    }
    (true, None)
}

/// Replay the withdraw queue: a request enters at TvWithdrawRequested, its
/// payout asset follows the latest TvPayoutAssetAmended, and it leaves at
/// TvWithdrawFulfilled — or, post-settlement, at TvSettlementRedeemed with
/// `from_queue`. Returns the survivors, ascending by global_seq.
fn pending_requests(mut events: Vec<protocol_types::events::IndexedEvent>) -> Vec<PendingRequest> {
    // The scan serves newest-first; the replay needs chain order.
    events.sort_by_key(|e| e.sequence);
    let mut open: std::collections::BTreeMap<u64, PendingRequest> =
        std::collections::BTreeMap::new();
    for ev in events {
        match ev.event {
            ChainEvent::TvWithdrawRequested(w) => {
                open.insert(
                    w.global_seq,
                    PendingRequest {
                        global_seq: w.global_seq,
                        lane: w.lane,
                        position_id: w.position_id,
                        recipient: w.recipient,
                        tranche: w.tranche,
                        capital_generation: w.capital_generation,
                        shares: w.shares,
                        basis: w.basis,
                        payout_asset: w.payout_asset,
                        requested_at_ms: w.requested_at_ms,
                    },
                );
            }
            ChainEvent::TvPayoutAssetAmended(a) => {
                // Rust keeps the v1 `seq` field name; semantically this is
                // the global sequence (WS-0 flag).
                if let Some(r) = open.get_mut(&a.seq) {
                    r.payout_asset = a.payout_asset;
                }
            }
            ChainEvent::TvWithdrawFulfilled(f) => {
                open.remove(&f.global_seq);
            }
            ChainEvent::TvSettlementRedeemed(s) if s.from_queue => {
                open.remove(&s.global_seq);
            }
            _ => {}
        }
    }
    open.into_values().collect()
}

// ── §3.4a waterfall + terminal settlement (SO-418) ────────────────────────

#[derive(Serialize)]
pub struct WaterfallResponse {
    pub nav_raw: String,
    pub senior_claim_raw: String,
    /// Senior principal without hurdle (the CappedParticipating cap
    /// reference); null for untranched vaults.
    pub senior_principal_basis_raw: Option<String>,
    /// Server-derived §3.4a decomposition (u128 floor math).
    pub preferred_raw: String,
    pub participation_raw: String,
    /// The on-chain waterfall split from the latest TvCapitalSynced.
    pub senior_nav_raw: String,
    pub junior_nav_raw: String,
    /// junior_nav × 1e4 / nav, floor; null at zero NAV.
    pub junior_buffer_bps: Option<i64>,
    pub target_junior_bps: i64,
    pub maintenance_junior_bps: i64,
    pub upside: String,
    pub residual_participation_bps: i64,
    pub total_return_cap_bps: i64,
    pub risk_state: String,
    pub risk_state_code: u8,
    pub senior_shares_raw: String,
    pub junior_shares_raw: String,
    pub updated_at_ms: i64,
}

/// The §3.4a decomposition: `(preferred, participation)` from
/// `(NAV, C, P)` and the upside mode. Exact spec formula, u128
/// intermediates, floor division.
fn derive_waterfall(
    nav: u128,
    senior_claim: u128,
    principal_basis: u128,
    upside_code: u8,
    participation_bps: u64,
    cap_bps: u64,
) -> (u128, u128) {
    let preferred = nav.min(senior_claim);
    let residual = nav - preferred;
    let participation = match upside_code {
        // PreferredOnly.
        0 => 0,
        // CappedParticipating: cap binds on total senior return relative
        // to principal basis.
        1 => {
            let share = residual.saturating_mul(participation_bps as u128) / BPS;
            let cap = (principal_basis.saturating_mul(cap_bps as u128) / BPS)
                .saturating_sub(preferred);
            share.min(cap)
        }
        // UncappedParticipating.
        _ => residual.saturating_mul(participation_bps as u128) / BPS,
    };
    (preferred, participation)
}

/// `GET /trading-vaults/:id/waterfall` — the §3.4a decomposition at the
/// latest capital sync. 404 until the first TvCapitalSynced (nothing to
/// decompose before an appraisal-driven sync).
pub async fn get_waterfall(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<WaterfallResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let vault = find_vault(&state, id).await?;
    // The newest TvCapitalSynced IS the latest waterfall run; its timestamp
    // dates the response.
    let events = state
        .indexer
        .recent_events_with_payload(
            &["TvCapitalSynced"],
            json!({ "vault_id": id.to_hex() }),
            1,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer capital-sync events query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let Some(latest) = events.last() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let ChainEvent::TvCapitalSynced(sync) = &latest.event else {
        return Err(StatusCode::NOT_FOUND);
    };
    // P comes from the vault row (the sync event doesn't carry it); it can
    // run slightly ahead of the sync, which only tightens the derived cap.
    let (preferred, participation) = derive_waterfall(
        sync.total_nav,
        sync.senior_claim,
        vault.senior_principal_basis,
        vault.upside_code,
        vault.residual_participation_bps,
        vault.total_return_cap_bps,
    );
    Ok(Json(WaterfallResponse {
        nav_raw: sync.total_nav.to_string(),
        senior_claim_raw: sync.senior_claim.to_string(),
        senior_principal_basis_raw: (vault.structure_code != 0)
            .then(|| vault.senior_principal_basis.to_string()),
        preferred_raw: preferred.to_string(),
        participation_raw: participation.to_string(),
        senior_nav_raw: sync.senior_nav.to_string(),
        junior_nav_raw: sync.junior_nav.to_string(),
        junior_buffer_bps: junior_buffer_bps(Some(sync.junior_nav), Some(sync.total_nav)),
        target_junior_bps: vault.target_junior_bps as i64,
        maintenance_junior_bps: vault.maintenance_junior_bps as i64,
        upside: upside_label(vault.upside_code).to_string(),
        residual_participation_bps: vault.residual_participation_bps as i64,
        total_return_cap_bps: vault.total_return_cap_bps as i64,
        risk_state: risk_state_label(sync.risk_state).to_string(),
        risk_state_code: sync.risk_state,
        senior_shares_raw: sync.senior_shares.to_string(),
        junior_shares_raw: sync.junior_shares.to_string(),
        updated_at_ms: latest.timestamp_ms as i64,
    }))
}

/// `GET /trading-vaults/:id/settlement` — the terminal settlement pool.
/// `{"settled": false}` (all other fields omitted) before the snapshot.
#[derive(Serialize)]
pub struct SettlementResponse {
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_nav_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub senior_pool_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub senior_supply_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub junior_pool_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub junior_supply_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_junior_generation: Option<i64>,
    /// Σ entitlement drawn from the pools (TvSettlementRedeemed sum).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redeemed_raw: Option<String>,
    /// senior_pool + junior_pool − redeemed: perpetual claims outstanding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outstanding_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_at_ms: Option<i64>,
}

pub async fn get_settlement(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<SettlementResponse>, StatusCode> {
    let id = ObjectId::from_hex(&vault_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let v = find_vault(&state, id).await?;
    if !v.settled {
        return Ok(Json(SettlementResponse {
            settled: false,
            final_nav_raw: None,
            senior_pool_raw: None,
            senior_supply_raw: None,
            junior_pool_raw: None,
            junior_supply_raw: None,
            active_junior_generation: None,
            redeemed_raw: None,
            outstanding_raw: None,
            snapshot_at_ms: None,
        }));
    }
    let pools = v.senior_pool.unwrap_or(0) as u128 + v.junior_pool.unwrap_or(0) as u128;
    Ok(Json(SettlementResponse {
        settled: true,
        final_nav_raw: v.settlement_final_nav.map(|n| n.to_string()),
        senior_pool_raw: v.senior_pool.map(|n| n.to_string()),
        senior_supply_raw: v.senior_supply.map(|n| n.to_string()),
        junior_pool_raw: v.junior_pool.map(|n| n.to_string()),
        junior_supply_raw: v.junior_supply.map(|n| n.to_string()),
        active_junior_generation: Some(v.active_junior_generation as i64),
        redeemed_raw: Some(v.settlement_redeemed.to_string()),
        outstanding_raw: Some(pools.saturating_sub(v.settlement_redeemed).to_string()),
        snapshot_at_ms: v.settlement_snapshot_at_ms.map(|t| t as i64),
    }))
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
        capital_structure: (v.structure_code != 0).then(|| CapitalStructureDto {
            senior_hurdle_bps_annual: v.senior_hurdle_bps_annual as i64,
            target_junior_bps: v.target_junior_bps as i64,
            maintenance_junior_bps: v.maintenance_junior_bps as i64,
            upside: upside_label(v.upside_code).to_string(),
            residual_participation_bps: v.residual_participation_bps as i64,
            total_return_cap_bps: v.total_return_cap_bps as i64,
        }),
        terms_version: v.terms_version as i64,
        spec_hash: v.spec_hash.clone(),
        risk_state: risk_state_label(v.risk_state).to_string(),
        risk_state_code: v.risk_state,
        curator_commitment_breached: v.curator_commitment_breached,
        senior_shares_raw: v.senior_shares.to_string(),
        junior_shares_raw: v.junior_shares.to_string(),
        senior_claim_raw: v.senior_claim.to_string(),
        senior_nav_raw: v.senior_nav.map(|n| n.to_string()),
        junior_nav_raw: v.junior_nav.map(|n| n.to_string()),
        senior_pps: v.latest_senior_pps_e12.map(|p| p as f64 / PPS_SCALE),
        senior_pps_raw: v.latest_senior_pps_e12.map(|p| p.to_string()),
        junior_pps: v.latest_junior_pps_e12.map(|p| p as f64 / PPS_SCALE),
        junior_pps_raw: v.latest_junior_pps_e12.map(|p| p.to_string()),
        junior_buffer_bps: junior_buffer_bps(v.junior_nav, v.latest_nav),
        impaired_since_ms: v.impaired_since_ms.map(|t| t as i64),
        active_junior_generation: v.active_junior_generation as i64,
        reset_proposal: reset_proposal_dto(v),
        settled: v.settled,
        lane_heads: LaneHeadsDto {
            senior: LaneCursorDto {
                head: v.senior_lane_head as i64,
                tail: v.senior_lane_tail as i64,
            },
            junior: LaneCursorDto {
                head: v.junior_lane_head as i64,
                tail: v.junior_lane_tail as i64,
            },
        },
    }
}

/// `junior_nav × 1e4 / nav`, floor; null before the first sync or at
/// zero NAV.
fn junior_buffer_bps(junior_nav: Option<u128>, nav: Option<u128>) -> Option<i64> {
    let junior = junior_nav?;
    let nav = nav?;
    if nav == 0 {
        return None;
    }
    Some((junior.saturating_mul(BPS) / nav) as i64)
}

/// The reset proposal is stored across six nullable columns; a proposal
/// exists iff they're all set (the indexer writes them atomically).
fn reset_proposal_dto(v: &TradingVault) -> Option<ResetProposalDto> {
    Some(ResetProposalDto {
        old_generation: v.reset_old_generation? as i64,
        proposed_at_ms: v.reset_proposed_at_ms? as i64,
        executable_at_ms: v.reset_executable_at_ms? as i64,
        recorded_nav_raw: v.reset_recorded_nav?.to_string(),
        recorded_senior_claim_raw: v.reset_recorded_senior_claim?.to_string(),
        recorded_required_deposit_raw: v.reset_recorded_required_deposit?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_types::asset::AssetType;
    use protocol_types::events::{
        IndexedEvent, TvCapitalSynced, TvDeposited, TvSettlementRedeemed, TvWithdrawFulfilled,
        TvWithdrawRequested,
    };

    fn oid(n: u8) -> ObjectId {
        ObjectId::from_hex(&format!("0x{:064x}", n)).unwrap()
    }

    fn addr(n: u8) -> SuiAddress {
        SuiAddress::from_hex(&format!("0x{:064x}", n)).unwrap()
    }

    fn ev(sequence: u64, event: ChainEvent) -> IndexedEvent {
        IndexedEvent {
            sequence,
            timestamp_ms: 1_000 + sequence,
            event,
        }
    }

    fn deposit(vault_id: ObjectId, tranche: u8, generation: u64, value: u64) -> ChainEvent {
        ChainEvent::TvDeposited(TvDeposited {
            vault_id,
            depositor: addr(2),
            commitment_position: None,
            position_id: oid(20),
            tranche,
            capital_generation: generation,
            asset: AssetType::new("9b::tusdc::TUSDC"),
            amount: value,
            value,
            shares: value as u128 * SHARE_OFFSET,
            tranche_shares: value as u128 * SHARE_OFFSET,
            locked_until_ms: 0,
        })
    }

    fn sync(
        vault_id: ObjectId,
        total_nav: u128,
        senior_nav: u128,
        junior_nav: u128,
        senior_shares: u128,
        junior_shares: u128,
        generation: u64,
    ) -> ChainEvent {
        ChainEvent::TvCapitalSynced(TvCapitalSynced {
            vault_id,
            total_nav,
            senior_nav,
            junior_nav,
            senior_claim: senior_nav,
            senior_shares,
            junior_shares,
            risk_state: 0,
            active_junior_generation: generation,
            curator_commitment_breached: false,
        })
    }

    /// Newest-first input (the scan's order) must come back as an ascending
    /// per-tranche curve: flow points price at value/shares, sync points at
    /// the tranche claim ratio, and a generation bump marks the first junior
    /// point of the new generation as `reset`.
    #[test]
    fn pps_points_labels_tranches_and_marks_resets() {
        let vault_id = oid(1);
        // Genesis junior deposit of 1_000_000 → 1e12 observed.
        let d_junior = deposit(vault_id, 2, 0, 1_000_000);
        let d_senior = deposit(vault_id, 1, 0, 500_000);
        // Sync: senior flat, junior +2%.
        let s1 = sync(
            vault_id,
            1_520_000,
            500_000,
            1_020_000,
            500_000 * SHARE_OFFSET as u128,
            1_000_000 * SHARE_OFFSET as u128,
            0,
        );
        // Post-reset sync: junior generation bumped, re-based to ~1.0.
        let s2 = sync(
            vault_id,
            700_000,
            500_000,
            200_000,
            500_000 * SHARE_OFFSET as u128,
            200_000 * SHARE_OFFSET as u128,
            1,
        );

        let newest_first = vec![ev(3, s2), ev(2, s1), ev(1, d_senior), ev(0, d_junior)];
        let points = pps_points(newest_first, 1);

        let got: Vec<(&str, &str, &str, bool)> = points
            .iter()
            .map(|p| {
                (
                    p.tranche.as_str(),
                    p.source.as_str(),
                    p.pps_raw.as_str(),
                    p.reset,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("junior", "deposit", "1000000000000", false),
                ("senior", "deposit", "1000000000000", false),
                // Claim ratio (nav+1)/(S+O): exactly 1e12 when nav = S/O
                // (the +1/+O virtual offsets cancel), floor-rounded on the
                // +2% junior mark.
                ("senior", "capital_sync", "1000000000000", false),
                ("junior", "capital_sync", "1019999980000", false),
                ("senior", "capital_sync", "1000000000000", false),
                ("junior", "capital_sync", "1000000000000", true),
            ],
        );
        let times: Vec<i64> = points.iter().map(|p| p.timestamp_ms).collect();
        assert_eq!(times, vec![1_000, 1_001, 1_002, 1_002, 1_003, 1_003]);
    }

    /// An untranched vault (structure 0) labels its single series
    /// `untranched`, pricing sync points off total NAV over the junior-held
    /// supply.
    #[test]
    fn pps_points_labels_untranched_series() {
        let vault_id = oid(1);
        let d = deposit(vault_id, 0, 0, 1_000_000);
        let s = sync(
            vault_id,
            1_020_000,
            0,
            0, // tranched split fields unused by the untranched path
            0,
            1_000_000 * SHARE_OFFSET as u128,
            0,
        );
        let points = pps_points(vec![ev(1, s), ev(0, d)], 0);
        let got: Vec<(&str, &str)> = points
            .iter()
            .map(|p| (p.tranche.as_str(), p.source.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("untranched", "deposit"), ("untranched", "capital_sync")],
        );
        assert_eq!(points[1].pps_raw, "1019999980000");
        assert!(!points.iter().any(|p| p.reset));
    }

    fn request(
        vault_id: ObjectId,
        global_seq: u64,
        lane: u8,
        tranche: u8,
        generation: u64,
        at: u64,
    ) -> ChainEvent {
        ChainEvent::TvWithdrawRequested(TvWithdrawRequested {
            vault_id,
            global_seq,
            lane,
            position_id: oid(30),
            recipient: addr(2),
            tranche,
            capital_generation: generation,
            shares: 100 * SHARE_OFFSET,
            basis: 100,
            payout_asset: AssetType::new("9b::tusdc::TUSDC"),
            requested_at_ms: at,
        })
    }

    /// The queue replay keeps unfulfilled requests, applies the latest
    /// payout-asset amendment, and drops both fulfilled and
    /// settlement-redeemed global_seqs (SO-370, SO-418).
    #[test]
    fn pending_requests_replays_the_withdraw_queue() {
        use protocol_types::events::TvPayoutAssetAmended;

        let vault_id = oid(1);
        let tusdc = AssetType::new("9b::tusdc::TUSDC");
        let tbtc = AssetType::new("9b::tbtc::TBTC");
        let amend = ChainEvent::TvPayoutAssetAmended(TvPayoutAssetAmended {
            vault_id,
            seq: 1,
            payout_asset: tbtc.clone(),
        });
        let fulfilled = ChainEvent::TvWithdrawFulfilled(TvWithdrawFulfilled {
            vault_id,
            global_seq: 0,
            lane: 0,
            recipient: addr(2),
            tranche: 1,
            capital_generation: 0,
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
            tranche_shares: 100 * SHARE_OFFSET,
        });
        let settled = ChainEvent::TvSettlementRedeemed(TvSettlementRedeemed {
            vault_id,
            position_id: oid(30),
            from_queue: true,
            global_seq: 2,
            recipient: addr(2),
            tranche: 2,
            capital_generation: 0,
            shares: 100 * SHARE_OFFSET,
            entitlement: 90,
            basis: 100,
            gross_fee: 0,
            protocol_cut: 0,
            curator_net: 0,
            payout: 90,
        });

        // Newest-first input, as the scan serves it.
        let events = vec![
            ev(5, settled),
            ev(4, fulfilled),
            ev(3, amend),
            ev(2, request(vault_id, 2, 1, 2, 0, 3_000)),
            ev(1, request(vault_id, 1, 1, 2, 0, 2_000)),
            ev(0, request(vault_id, 0, 0, 1, 0, 1_000)),
        ];
        let pending = pending_requests(events);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].global_seq, 1);
        assert_eq!(pending[0].lane, 1);
        assert_eq!(pending[0].tranche, 2);
        assert_eq!(pending[0].payout_asset, tbtc);
        assert_eq!(pending[0].requested_at_ms, 2_000);
    }

    /// §7 action matrix: a wiped junior request settles at zero; the junior
    /// lane is blocked in any non-Healthy risk state; senior always flows.
    #[test]
    fn request_payability_follows_the_action_matrix() {
        let jr = |generation| PendingRequest {
            global_seq: 1,
            lane: 1,
            position_id: oid(30),
            recipient: addr(2),
            tranche: 2,
            capital_generation: generation,
            shares: 1,
            basis: 1,
            payout_asset: AssetType::new("9b::tusdc::TUSDC"),
            requested_at_ms: 0,
        };
        let sr = PendingRequest {
            lane: 0,
            tranche: 1,
            ..jr(0)
        };
        // Healthy: everything pays.
        assert_eq!(request_payability(0, 0, &jr(0)), (true, None));
        assert_eq!(request_payability(0, 0, &sr), (true, None));
        // Wiped generation loses even while Healthy (post-reset).
        assert_eq!(
            request_payability(0, 1, &jr(0)),
            (false, Some("wiped_generation"))
        );
        // CoverageBreach blocks the junior lane only.
        assert_eq!(
            request_payability(1, 0, &jr(0)),
            (false, Some("junior_lane_blocked"))
        );
        assert_eq!(request_payability(1, 0, &sr), (true, None));
    }

    /// Spec §3 worked examples: NAV 1,000,000 / C 400,000 / P 400,000.
    #[test]
    fn derive_waterfall_matches_the_spec_worked_examples() {
        // PreferredOnly ⇒ (400,000 / 600,000).
        assert_eq!(
            derive_waterfall(1_000_000, 400_000, 400_000, 0, 0, 0),
            (400_000, 0)
        );
        // Uncapped 30% ⇒ participation 180,000 (senior 580,000).
        assert_eq!(
            derive_waterfall(1_000_000, 400_000, 400_000, 2, 3_000, 0),
            (400_000, 180_000)
        );
        // Capped 50% with 120% cap and C accrued to 410,000 ⇒
        // min(295,000, 480,000−410,000) = 70,000 (senior 480,000).
        assert_eq!(
            derive_waterfall(1_000_000, 410_000, 400_000, 1, 5_000, 12_000),
            (410_000, 70_000)
        );
        // Boundary: NAV < C ⇒ (NAV, 0) in every mode.
        assert_eq!(
            derive_waterfall(300_000, 400_000, 400_000, 2, 3_000, 0),
            (300_000, 0)
        );
        // Boundary: NAV 0 ⇒ (0, 0).
        assert_eq!(derive_waterfall(0, 400_000, 400_000, 1, 5_000, 12_000), (0, 0));
    }

    /// Estimates use the claim ratio `shares × (nav+1) / (S+O)`, fall back
    /// to observed pps before the first sync, and go null with neither.
    #[test]
    fn position_estimates_price_from_the_tranche_ratio() {
        let shares = 100_000 * SHARE_OFFSET; // 100k units at genesis pricing
        let supply = 1_000_000 * SHARE_OFFSET;
        // Tranche NAV grew 2%: 100k of shares → ~102k value (floor division
        // over the +1/+O virtual offsets shaves the last unit), profit
        // value−basis, fee 10% of profit (floor).
        let (v, p, f) = position_estimates(Some(1_020_000), supply, None, 1_000, shares, 100_000);
        assert_eq!(v, Some(101_999));
        assert_eq!(p, Some(1_999));
        assert_eq!(f, Some(199));
        // No sync yet: observed pps 1e12 prices at par; recovery below basis
        // is never charged.
        let (v, p, f) = position_estimates(None, supply, Some(PPS_E12), 1_000, shares, 120_000);
        assert_eq!(v, Some(100_000));
        assert_eq!(p, Some(0));
        assert_eq!(f, Some(0));
        // Neither ratio → null estimates.
        assert_eq!(position_estimates(None, supply, None, 1_000, shares, 0), (None, None, None));
    }

    #[test]
    fn junior_buffer_bps_floors_and_guards_zero_nav() {
        assert_eq!(junior_buffer_bps(Some(250_000), Some(1_000_000)), Some(2_500));
        assert_eq!(junior_buffer_bps(Some(1), Some(0)), None);
        assert_eq!(junior_buffer_bps(None, Some(1)), None);
        assert_eq!(junior_buffer_bps(Some(1), None), None);
    }
}
