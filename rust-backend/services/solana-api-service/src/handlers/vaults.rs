//! Covered-call vault endpoints:
//!
//!   - `GET /vaults`                     — list with pps / tvl / APY
//!   - `GET /vaults/:id`                 — one vault (+ live round state)
//!   - `GET /vaults/:id/rounds`          — round history (the track record)
//!   - `GET /vaults/:id/apy`             — realized (indexer) + predicted
//!   - `GET /vaults/:id/receipts?owner=` — a user's deposit/withdraw
//!     receipt aggregates with derived claimability
//!
//! All reads are JIT GraphQL queries to the indexer. `GET /vaults/:id`
//! additionally does one best-effort `getAccountInfo` (see [`crate::solana_rpc`])
//! for the live round state — phase, selling window, open-RFQ counters,
//! config guardrails. Solana note: the Sui vault's balance fields
//! (deployable, proceeds, withdrawal pool, claimable/queued shares) live
//! in PDA-seeded token accounts on Solana and are not part of this read.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::handlers::buckets::strike_raw_to_usd;
use crate::ids;
use crate::solana_rpc::{self, VaultLive};
use crate::state::{AppState, Vault, VaultRound};

/// `options_math::PPS_SCALE` — pps is a 1e12-scaled underlying-per-share.
const PPS_SCALE: f64 = 1e12;
const MS_PER_YEAR: f64 = 365.0 * 86_400_000.0;

#[derive(Serialize)]
pub struct VaultDto {
    pub vault_id: String,
    pub underlying_symbol: String,
    pub underlying_decimals: Option<u8>,
    pub underlying_mint: String,
    pub settlement_symbol: String,
    pub settlement_decimals: Option<u8>,
    pub settlement_mint: String,
    /// SPL mint of the vault's share token.
    pub share_mint: String,
    /// Current round (last finalized + 1; 0 = pre-genesis).
    pub round: i64,
    pub current_bucket: Option<String>,
    /// Underlying-per-share at the last finalize (PPS_SCALE-adjusted).
    pub pps: Option<f64>,
    pub pps_raw: Option<String>,
    /// shares × pps + queued deposits, in whole underlying units.
    pub tvl: Option<f64>,
    pub tvl_raw: Option<String>,
    pub total_shares_raw: String,
    pub pending_deposits_raw: String,
    /// Ribbon-formula APY over the last two finalized rounds; null until
    /// two rounds exist.
    pub apy: Option<f64>,
    pub deposits_paused: bool,
    /// Live round phase. Ground-truth from the on-chain `Phase` enum when
    /// the RPC read succeeds, else derived from `current_bucket` + the
    /// current round's expiry: `active` (selling/holding) vs `settling`
    /// (between rounds / past expiry, redeeming).
    pub phase: String,
    // Active VaultConfig (consumer-facing subset), in display units.
    // `None` until the config-carrying events are indexed for this vault.
    pub mgmt_fee_pct: Option<f64>,
    pub perf_fee_pct: Option<f64>,
    pub min_strike_over_spot_pct: Option<f64>,
    pub max_strike_over_spot_pct: Option<f64>,
    pub round_ms: Option<i64>,
    pub selling_window_ms: Option<i64>,
    // Config slice guardrails, from the live account read. `None` on the
    // list endpoint and whenever the RPC read is unavailable.
    pub max_slice_amount_raw: Option<String>,
    pub max_open_rfqs: Option<i64>,
    // Live round state (`getAccountInfo`). All `None` on the list
    // endpoint and whenever the RPC read is unavailable — never a hard
    // error.
    pub selling_ends_ms: Option<i64>,
    pub current_expiry_ms: Option<i64>,
    pub open_rfqs: Option<i64>,
    pub open_swap_rfqs: Option<i64>,
    /// Mgmt + perf fees charged since inception, summed over finalized
    /// rounds. Underlying-denominated.
    pub total_fees: Option<f64>,
    pub total_fees_raw: String,
}

#[derive(Serialize)]
pub struct VaultsResponse {
    pub vaults: Vec<VaultDto>,
}

#[derive(Serialize)]
pub struct VaultRoundDto {
    pub round: i64,
    pub bucket_id: Option<String>,
    /// Strike in USD-cross whole units (null when decimals are unknown).
    pub strike: Option<f64>,
    pub strike_raw: Option<String>,
    pub strike_scale: Option<u8>,
    pub expiry_ms: Option<i64>,
    pub pps: Option<f64>,
    pub pps_raw: Option<String>,
    pub aum_raw: Option<String>,
    pub shares_raw: Option<String>,
    pub premium_collected_raw: Option<String>,
    pub mgmt_fee_raw: Option<String>,
    pub perf_fee_raw: Option<String>,
    pub finalized_at_ms: Option<i64>,
}

#[derive(Serialize)]
pub struct VaultRoundsResponse {
    pub vault_id: String,
    pub rounds: Vec<VaultRoundDto>,
}

#[derive(Deserialize)]
pub struct ReceiptsQuery {
    pub owner: Option<String>,
}

#[derive(Serialize)]
pub struct VaultReceiptDto {
    pub owner: String,
    pub round: i64,
    /// deposit | withdraw.
    pub kind: String,
    /// Queued underlying (deposits) / escrowed shares (withdrawals).
    pub amount_raw: String,
    /// Already claimed / completed.
    pub settled_raw: String,
    /// pending | claimable | settled — derived from the vault's current
    /// round (a deposit at round r claims once pps[r−1] exists; a
    /// withdrawal at round r pays once round r finalizes).
    pub status: String,
}

#[derive(Serialize)]
pub struct VaultReceiptsResponse {
    pub vault_id: String,
    pub receipts: Vec<VaultReceiptDto>,
}

pub async fn list_vaults(
    State(state): State<Arc<AppState>>,
) -> Result<Json<VaultsResponse>, StatusCode> {
    let vaults = state.indexer.vaults().await.map_err(|e| {
        tracing::warn!(error = %e, "indexer vaults query failed");
        StatusCode::BAD_GATEWAY
    })?;
    let mut out = Vec::with_capacity(vaults.len());
    for v in vaults {
        // APY needs the round history; vault count is a handful, so the
        // extra query per vault is fine at this rate.
        let rounds = state.indexer.vault_rounds(&v.vault_id).await.map_err(|e| {
            tracing::warn!(error = %e, "indexer vault_rounds query failed");
            StatusCode::BAD_GATEWAY
        })?;
        // The list view skips the per-vault RPC read; live round state is
        // a detail-page concern.
        out.push(vault_dto(&state, &v, &rounds, None));
    }
    Ok(Json(VaultsResponse { vaults: out }))
}

pub async fn get_vault(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<VaultDto>, StatusCode> {
    parse_vault_id(&vault_id)?;
    let vault = state
        .indexer
        .vault(&vault_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer vault query failed");
            StatusCode::BAD_GATEWAY
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let rounds = state.indexer.vault_rounds(&vault_id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer vault_rounds query failed");
        StatusCode::BAD_GATEWAY
    })?;
    // Live round state is best-effort: an RPC hiccup degrades to omitting
    // the live fields rather than failing the page.
    let live = match solana_rpc::fetch_vault_live(&state.http, &state.solana_rpc_url, &vault_id)
        .await
    {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!(error = %e, "vault live read failed; serving without live state");
            None
        }
    };
    Ok(Json(vault_dto(&state, &vault, &rounds, live.as_ref())))
}

pub async fn list_vault_rounds(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<VaultRoundsResponse>, StatusCode> {
    parse_vault_id(&vault_id)?;
    let vault = state
        .indexer
        .vault(&vault_id)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let rounds = state.indexer.vault_rounds(&vault_id).await.map_err(|e| {
        tracing::warn!(error = %e, "indexer vault_rounds query failed");
        StatusCode::BAD_GATEWAY
    })?;
    let decimals = decimals_for(&state, &vault);
    let rounds = rounds
        .into_iter()
        .map(|r| VaultRoundDto {
            round: r.round as i64,
            bucket_id: r.bucket_id,
            strike: match (r.strike, r.strike_scale, decimals) {
                (Some(k), Some(scale), Some((u, s))) => Some(strike_raw_to_usd(k, scale, u, s)),
                _ => None,
            },
            strike_raw: r.strike.map(|k| k.to_string()),
            strike_scale: r.strike_scale,
            expiry_ms: r.expiry_ms.map(|v| v as i64),
            pps: r.pps.map(|p| p as f64 / PPS_SCALE),
            pps_raw: r.pps.map(|p| p.to_string()),
            aum_raw: r.aum.map(|v| v.to_string()),
            shares_raw: r.shares.map(|v| v.to_string()),
            premium_collected_raw: r.premium_collected.map(|v| v.to_string()),
            mgmt_fee_raw: r.mgmt_fee.map(|v| v.to_string()),
            perf_fee_raw: r.perf_fee.map(|v| v.to_string()),
            finalized_at_ms: r.finalized_at_ms.map(|v| v as i64),
        })
        .collect();
    Ok(Json(VaultRoundsResponse { vault_id, rounds }))
}

/// One point on either the realized or predicted APY curve. `kind` /
/// `confidence` are populated for predicted points only.
#[derive(Serialize)]
pub struct ApyPointDto {
    pub t_ms: i64,
    pub apy: f64,
    /// Low/high of the predicted range — predicted points only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apy_low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apy_high: Option<f64>,
    /// Per-round probability the option is assigned — predicted only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_prob: Option<f64>,
    /// Per-round (not annualized) net yield if assigned — predicted only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downside_round_yield: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Serialize)]
pub struct VaultApyResponse {
    pub vault_id: String,
    /// Annualized pps growth per finalized round (chain data, computed
    /// indexer-side via `vaultApy`).
    pub realized: Vec<ApyPointDto>,
    /// Forward-looking premium-yield APY (solana-price-charting's apy
    /// sampler). Empty when it isn't configured or has no snapshot yet.
    pub predicted: Vec<ApyPointDto>,
}

/// Worker read-API shapes (`GET /vault-apy/:id`).
#[derive(Deserialize)]
struct WorkerApy {
    predicted: Vec<WorkerPoint>,
}
#[derive(Deserialize)]
struct WorkerPoint {
    t_ms: i64,
    apy: f64,
    apy_low: f64,
    apy_high: f64,
    assignment_prob: f64,
    downside_round_yield: f64,
    kind: String,
    confidence: f64,
}

/// `GET /vaults/:id/apy` — realized (indexer) + predicted (worker),
/// composed.
pub async fn get_vault_apy(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
) -> Result<Json<VaultApyResponse>, StatusCode> {
    parse_vault_id(&vault_id)?;
    let realized = state
        .indexer
        .vault_apy(&vault_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer vault_apy query failed");
            StatusCode::BAD_GATEWAY
        })?
        .into_iter()
        .map(|p| ApyPointDto {
            t_ms: p.t_ms as i64,
            apy: p.apy,
            apy_low: None,
            apy_high: None,
            assignment_prob: None,
            downside_round_yield: None,
            kind: None,
            confidence: None,
        })
        .collect();

    // Predicted is best-effort: a worker outage degrades to realized-only
    // rather than failing the whole endpoint.
    let predicted = fetch_predicted(&state, &vault_id).await;

    Ok(Json(VaultApyResponse {
        vault_id,
        realized,
        predicted,
    }))
}

async fn fetch_predicted(state: &AppState, vault_id: &str) -> Vec<ApyPointDto> {
    let Some(base) = state.derived_metrics_url.as_deref() else {
        return Vec::new();
    };
    let url = format!("{base}/vault-apy/{vault_id}");
    match state.http.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<WorkerApy>().await {
            Ok(body) => body
                .predicted
                .into_iter()
                .map(|p| ApyPointDto {
                    t_ms: p.t_ms,
                    apy: p.apy,
                    apy_low: Some(p.apy_low),
                    apy_high: Some(p.apy_high),
                    assignment_prob: Some(p.assignment_prob),
                    downside_round_yield: Some(p.downside_round_yield),
                    kind: Some(p.kind),
                    confidence: Some(p.confidence),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "decoding derived-metrics predicted apy");
                Vec::new()
            }
        },
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "derived-metrics apy non-2xx");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(error = %e, "derived-metrics apy request failed");
            Vec::new()
        }
    }
}

pub async fn list_vault_receipts(
    State(state): State<Arc<AppState>>,
    Path(vault_id): Path<String>,
    Query(q): Query<ReceiptsQuery>,
) -> Result<Json<VaultReceiptsResponse>, StatusCode> {
    parse_vault_id(&vault_id)?;
    if let Some(o) = q.owner.as_deref() {
        if !ids::is_pubkey(o) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let vault = state
        .indexer
        .vault(&vault_id)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let receipts = state
        .indexer
        .vault_receipts(&vault_id, q.owner.as_deref())
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "indexer vault_receipts query failed");
            StatusCode::BAD_GATEWAY
        })?;
    let receipts = receipts
        .into_iter()
        .map(|r| {
            let outstanding = r.amount.saturating_sub(r.settled);
            let payable = match r.kind.as_str() {
                // Deposit at round r claims at pps[r−1], which exists once
                // the vault's current round reached r.
                "deposit" => vault.round >= r.round,
                // Withdrawal at round r pays once round r finalized.
                _ => vault.round > r.round,
            };
            let status = if outstanding == 0 {
                "settled"
            } else if payable {
                "claimable"
            } else {
                "pending"
            };
            VaultReceiptDto {
                owner: r.owner,
                round: r.round as i64,
                kind: r.kind,
                amount_raw: r.amount.to_string(),
                settled_raw: r.settled.to_string(),
                status: status.to_string(),
            }
        })
        .collect();
    Ok(Json(VaultReceiptsResponse { vault_id, receipts }))
}

fn parse_vault_id(s: &str) -> Result<(), StatusCode> {
    if ids::is_pubkey(s) {
        Ok(())
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

fn decimals_for(state: &AppState, v: &Vault) -> Option<(u8, u8)> {
    let u = state.catalog.lookup(&v.underlying_mint)?.decimals;
    let s = state.catalog.lookup(&v.settlement_mint)?.decimals;
    Some((u, s))
}

fn vault_dto(
    state: &AppState,
    v: &Vault,
    rounds: &[VaultRound],
    live: Option<&VaultLive>,
) -> VaultDto {
    let u_meta = state.catalog.lookup(&v.underlying_mint);
    let s_meta = state.catalog.lookup(&v.settlement_mint);
    let u_dec = u_meta.map(|m| m.decimals);

    // tvl = shares × pps / PPS_SCALE + queued deposits (underlying units).
    let tvl_raw = v.latest_pps.map(|pps| {
        let held = (v.total_shares as u128).saturating_mul(pps) / PPS_SCALE as u128;
        held as u64 + v.pending_deposits
    });

    // Fees since inception: mgmt + perf summed over rounds, underlying units.
    let total_fees_raw: u128 = rounds
        .iter()
        .map(|r| r.mgmt_fee.unwrap_or(0) as u128 + r.perf_fee.unwrap_or(0) as u128)
        .sum();

    VaultDto {
        vault_id: v.vault_id.clone(),
        underlying_symbol: u_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| v.underlying_mint.clone()),
        underlying_decimals: u_dec,
        underlying_mint: v.underlying_mint.clone(),
        settlement_symbol: s_meta
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| v.settlement_mint.clone()),
        settlement_decimals: s_meta.map(|m| m.decimals),
        settlement_mint: v.settlement_mint.clone(),
        share_mint: v.share_mint.clone(),
        round: v.round as i64,
        current_bucket: v.current_bucket.clone(),
        pps: v.latest_pps.map(|p| p as f64 / PPS_SCALE),
        pps_raw: v.latest_pps.map(|p| p.to_string()),
        tvl: match (tvl_raw, u_dec) {
            (Some(t), Some(d)) => Some(t as f64 / 10f64.powi(d as i32)),
            _ => None,
        },
        tvl_raw: tvl_raw.map(|t| t.to_string()),
        total_shares_raw: v.total_shares.to_string(),
        pending_deposits_raw: v.pending_deposits.to_string(),
        apy: apy_from_rounds(rounds),
        deposits_paused: v.deposits_paused,
        // Ground-truth phase from the live read; heuristic fallback else.
        phase: live
            .map(|l| l.phase.clone())
            .unwrap_or_else(|| vault_phase(v, rounds)),
        mgmt_fee_pct: v.mgmt_fee_bps_annual.map(|b| b as f64 / 100.0),
        perf_fee_pct: v.perf_fee_bps.map(|b| b as f64 / 100.0),
        min_strike_over_spot_pct: v.min_strike_bps_over_spot.map(|b| b as f64 / 100.0),
        max_strike_over_spot_pct: v.max_strike_bps_over_spot.map(|b| b as f64 / 100.0),
        round_ms: v.round_ms.map(|m| m as i64),
        selling_window_ms: v.selling_window_ms.map(|m| m as i64),
        max_slice_amount_raw: live.map(|l| l.max_slice_amount.to_string()),
        max_open_rfqs: live.map(|l| l.max_open_rfqs as i64),
        selling_ends_ms: live.map(|l| l.selling_ends_ms as i64),
        current_expiry_ms: live.map(|l| l.current_expiry_ms as i64),
        open_rfqs: live.map(|l| l.open_rfqs as i64),
        open_swap_rfqs: live.map(|l| l.open_swap_rfqs as i64),
        total_fees: u_dec.map(|d| total_fees_raw as f64 / 10f64.powi(d as i32)),
        total_fees_raw: total_fees_raw.to_string(),
    }
}

/// Round phase from indexed state (fallback when the live read is
/// unavailable): a selected bucket whose current round hasn't passed
/// expiry is `active` (selling/holding); otherwise the vault is
/// `settling` (between rounds, or past expiry redeeming).
fn vault_phase(v: &Vault, rounds: &[VaultRound]) -> String {
    if v.current_bucket.is_none() {
        return "settling".to_string();
    }
    let expiry = rounds
        .iter()
        .find(|r| r.round == v.round)
        .and_then(|r| r.expiry_ms);
    let now_ms = chrono::Utc::now().timestamp_millis();
    match expiry {
        Some(e) if now_ms >= e as i64 => "settling".to_string(),
        _ => "active".to_string(),
    }
}

/// Ribbon-style APY: the latest pps-to-pps return, annualized by the
/// observed spacing between the two finalizes. Needs two finalized rounds.
fn apy_from_rounds(rounds: &[VaultRound]) -> Option<f64> {
    let finalized: Vec<&VaultRound> = rounds
        .iter()
        .filter(|r| r.pps.is_some() && r.finalized_at_ms.is_some())
        .collect();
    let [.., prev, last] = finalized.as_slice() else {
        return None;
    };
    let (p0, p1) = (prev.pps? as f64, last.pps? as f64);
    let (t0, t1) = (prev.finalized_at_ms? as f64, last.finalized_at_ms? as f64);
    if p0 <= 0.0 || t1 <= t0 {
        return None;
    }
    let periods_per_year = MS_PER_YEAR / (t1 - t0);
    Some((p1 / p0).powf(periods_per_year) - 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(n: u64, pps: u128, at_ms: u64) -> VaultRound {
        VaultRound {
            vault_id: "Vlt111".to_string(),
            round: n,
            bucket_id: None,
            strike: None,
            strike_scale: None,
            expiry_ms: None,
            selling_ends_ms: None,
            spot: None,
            spot_scale: None,
            pps: Some(pps),
            aum: None,
            shares: None,
            premium_collected: None,
            mgmt_fee: None,
            perf_fee: None,
            finalized_at_ms: Some(at_ms),
        }
    }

    #[test]
    fn apy_annualizes_weekly_pps_growth() {
        const WEEK: u64 = 7 * 86_400_000;
        // +0.2% per week ≈ (1.002)^52.14 − 1 ≈ 10.97%.
        let rounds = vec![round(1, 1_000_000_000_000, 0), round(2, 1_002_000_000_000, WEEK)];
        let apy = apy_from_rounds(&rounds).unwrap();
        assert!((apy - 0.1097).abs() < 0.002, "apy {apy}");
    }

    #[test]
    fn apy_needs_two_finalized_rounds() {
        assert!(apy_from_rounds(&[]).is_none());
        assert!(apy_from_rounds(&[round(1, 1_000_000_000_000, 0)]).is_none());
        // A selected-but-unfinalized round doesn't count.
        let mut r2 = round(2, 0, 0);
        r2.pps = None;
        r2.finalized_at_ms = None;
        assert!(apy_from_rounds(&[round(1, 1_000_000_000_000, 0), r2]).is_none());
    }

    #[test]
    fn vault_id_must_be_a_pubkey() {
        assert!(parse_vault_id("So11111111111111111111111111111111111111112").is_ok());
        assert!(parse_vault_id("0xdeadbeef").is_err());
    }
}
