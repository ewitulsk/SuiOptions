//! Vault-APY sampler (folded in from the retired derived-metric-worker, SO).
//!
//! DEPRECATED (SO-332) — no longer spawned by `main.rs`. The covered-call
//! vault product is retired, so there are no vaults to sample. Kept in-tree,
//! compiling, alongside the `vault_{predicted,realized}_apy` hypertables that
//! still hold the historical series.
//!
//! Every `tick_interval`, for each covered-call vault:
//!   - **Predicted** (active vaults only) — Tier 1 annualizes the premium the
//!     vault is on track to collect this round from its live RFQ auctions;
//!     Tier 2 prices the next K rounds with Black–Scholes at the keeper's
//!     delta-target strike, net of fees. Each point is APPENDED to
//!     `vault_predicted_apy`, so the prediction history is retained.
//!   - **Realized** (every vault) — the indexer's `vaultApy` series (annualized
//!     pps growth per finalized round) is mirrored into `vault_realized_apy`,
//!     idempotent on (vault, round). The indexer stays the source of the
//!     formula; we persist it so it becomes a queryable time-series.
//!
//! Unlike the fill watcher there is no cursor: predictions are point-in-time
//! recomputes and realized rounds are idempotent, so history simply starts when
//! sampling starts. Per-vault failures are logged and skipped; only a failed
//! vault *list* or DB write fails the tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use tracing::{error, info, warn};

use indexer_graphql::{IndexerClient, Vault};
use token_info_client::{Snapshot, SupportedToken};

use crate::apy::compute::{self, PredictionPoint, VaultInputs};
use crate::apy::spot;
use crate::config::{ModelConfig, PythConfig};
use crate::db::models::{PredictedApyRow, RealizedApyRow};
use crate::state::AppState;

/// `vault.move::PPS_SCALE` — pps is a 1e12-scaled underlying-per-share.
const PPS_SCALE: u128 = 1_000_000_000_000;

pub struct ApySamplerParams {
    pub state: Arc<AppState>,
    pub indexer: IndexerClient,
    /// Token catalog (decimals + Pyth feed ids), fetched once at boot.
    pub snapshot: Snapshot,
    /// The single Pyth gateway: spot prices + cached/paced realized vol.
    pub oracle: oracle_client::OracleClient,
    pub tick_interval: Duration,
    pub pyth: PythConfig,
    pub model: ModelConfig,
}

pub fn spawn(p: ApySamplerParams) {
    tokio::spawn(async move {
        run(p).await;
    });
}

async fn run(p: ApySamplerParams) {
    let mut ticker = tokio::time::interval(p.tick_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut consecutive_failures: u32 = 0;

    loop {
        ticker.tick().await;
        let started = Instant::now();
        match tick_once(&p).await {
            Ok((predicted, realized)) => {
                consecutive_failures = 0;
                metrics::counter!("price_charting_apy_tick_total", "outcome" => "ok").increment(1);
                metrics::gauge!("price_charting_apy_last_success_timestamp_seconds")
                    .set(now_secs() as f64);
                info!(predicted, realized, "apy tick complete");
            }
            Err(e) => {
                consecutive_failures += 1;
                metrics::counter!("price_charting_apy_tick_total", "outcome" => "error")
                    .increment(1);
                // Page only after repeated failures so a transient
                // indexer/feed blip doesn't wake anyone.
                if consecutive_failures >= 3 {
                    error!(
                        alert_id = "apy-sampler-failing",
                        consecutive_failures,
                        error = %format!("{e:#}"),
                        "apy sampler tick failing repeatedly"
                    );
                } else {
                    warn!(error = %format!("{e:#}"), consecutive_failures, "apy tick errored");
                }
            }
        }
        metrics::histogram!("price_charting_apy_tick_duration_seconds")
            .record(started.elapsed().as_secs_f64());
    }
}

/// One recompute pass. Returns `(predicted_rows_written, realized_rows_written)`.
async fn tick_once(p: &ApySamplerParams) -> Result<(usize, usize)> {
    let vaults = p.indexer.vaults().await.context("listing vaults")?;

    // ── Realized: mirror the indexer's series for EVERY vault (including
    // paused ones — their finalized track record is historical truth). ──
    let mut realized_rows: Vec<RealizedApyRow> = Vec::new();
    for vault in &vaults {
        let vid = vault.vault_id.to_hex();
        match p.indexer.vault_apy(vault.vault_id).await {
            Ok(points) => {
                for pt in points {
                    realized_rows.push(RealizedApyRow {
                        time: ms_to_dt(pt.t_ms as i64),
                        vault_id: vid.clone(),
                        round: pt.round as i64,
                        apy: pt.apy,
                    });
                }
            }
            Err(e) => warn!(vault = %vid, error = %format!("{e:#}"), "realized apy fetch skipped"),
        }
    }

    // ── Predicted: active vaults only — a paused (decommissioned) vault
    // shouldn't get forward-looking APY. ──
    let active: Vec<&Vault> = vaults.iter().filter(|v| !v.deposits_paused).collect();

    // Resolve realized vol once for the whole tick: collect the distinct
    // (beta) feed ids across every active vault, then ask oracle-service for
    // their sigmas in one shared bulk pass (it maps beta→stable, caches, paces).
    let mut feeds: Vec<oracle_client::PriceFeedId> = Vec::new();
    for vault in &active {
        if let Some(u_tok) = p.snapshot.token_by_coin_type(vault.underlying_type.as_str()) {
            if let Ok(feed) = u_tok.pyth_feed() {
                if !feeds.contains(&feed) {
                    feeds.push(feed);
                }
            }
        }
    }
    let sigmas = p
        .oracle
        .realized_vol_bulk(&feeds, p.pyth.vol_window_days)
        .await
        .unwrap_or_else(|e| {
            warn!(error = %format!("{e:#}"), "oracle realized-vol bulk failed; Tier 2 skipped this tick");
            HashMap::new()
        });

    let now = Utc::now();
    let mut predicted_rows: Vec<PredictedApyRow> = Vec::new();
    for vault in &active {
        match compute_vault(p, vault, &sigmas).await {
            Ok(points) => {
                metrics::counter!("price_charting_apy_vault_compute_total", "outcome" => "ok")
                    .increment(1);
                let vid = vault.vault_id.to_hex();
                for pt in &points {
                    metrics::gauge!(
                        "price_charting_predicted_apy",
                        "vault" => vid.clone(),
                        "kind" => pt.kind,
                    )
                    .set(pt.apy);
                    predicted_rows.push(PredictedApyRow {
                        time: now,
                        vault_id: vid.clone(),
                        kind: pt.kind.to_string(),
                        horizon: pt.horizon,
                        t_ms: pt.t_ms,
                        apy: pt.apy,
                        apy_low: pt.apy_low,
                        apy_high: pt.apy_high,
                        assignment_prob: pt.assignment_prob,
                        downside_round_yield: pt.downside_round_yield,
                        confidence: pt.confidence,
                    });
                }
            }
            Err(e) => {
                metrics::counter!("price_charting_apy_vault_compute_total", "outcome" => "error")
                    .increment(1);
                warn!(vault = %vault.vault_id.to_hex(), error = %format!("{e:#}"), "vault prediction skipped");
            }
        }
    }

    // Persist on the blocking pool. A write failure is page-worthy: the read
    // API serves the prior snapshot until it recovers.
    let repo = p.state.repo.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
        let pred = repo.insert_predicted_apy(&predicted_rows)?;
        let real = repo.insert_realized_apy(&realized_rows)?;
        Ok((pred, real))
    })
    .await
    .context("join apy upsert task")?
    .map_err(|e| {
        error!(alert_id = "apy-db-write-failed", error = %format!("{e:#}"), "failed to persist vault apy");
        e
    })?;
    metrics::counter!("price_charting_predicted_apy_written_total").increment(written.0 as u64);
    metrics::counter!("price_charting_realized_apy_written_total").increment(written.1 as u64);
    Ok(written)
}

/// Resolve market data + vault state and compute the predicted points for one
/// vault. Missing inputs are counted and surfaced as errors (skip the vault).
async fn compute_vault(
    p: &ApySamplerParams,
    vault: &Vault,
    sigmas: &HashMap<oracle_client::PriceFeedId, Result<f64>>,
) -> Result<Vec<PredictionPoint>> {
    let u_tok = token_for(&p.snapshot, vault.underlying_type.as_str(), "underlying")?;
    let s_tok = token_for(&p.snapshot, vault.settlement_type.as_str(), "settlement")?;

    // Spot is required for both tiers (premium → underlying conversion).
    let spot = spot::resolve_cross(
        &p.oracle,
        u_tok,
        s_tok,
        p.pyth.max_publish_lag_ms,
        p.pyth.max_conf_bps,
    )
    .await
    .inspect_err(|_| missing("spot"))
    .context("resolving spot cross")?;

    // Vol only gates Tier 2; a vol failure still yields the Tier-1 point. Sigma
    // was resolved for the whole tick (bulk + cached); look this vault's feed up.
    let sigma = match u_tok.pyth_feed().ok().and_then(|f| sigmas.get(&f)) {
        Some(Ok(s)) => *s,
        other => {
            missing("vol");
            let err = match other {
                Some(Err(e)) => format!("{e:#}"),
                _ => "no benchmark feed for underlying".to_string(),
            };
            warn!(vault = %vault.vault_id.to_hex(), error = %err, "vol unavailable; Tier 2 forecast skipped");
            0.0
        }
    };

    // Fees come from the vault's served on-chain config — never guessed. A
    // vault whose config hasn't been indexed yet is skipped (not faked).
    let (Some(perf_bps), Some(mgmt_bps)) = (vault.perf_fee_bps, vault.mgmt_fee_bps_annual) else {
        missing("config");
        anyhow::bail!("vault config not indexed yet (no fees served)");
    };

    let rounds = p.indexer.vault_rounds(vault.vault_id).await.context("vault rounds")?;
    // Round length: prefer the served config, else derive from observed
    // finalize spacing. Both are real; if neither exists, skip the vault.
    let round_ms = match vault.round_ms {
        Some(ms) if ms > 0 => ms,
        _ => match compute::median_round_ms(
            rounds.iter().filter_map(|r| r.finalized_at_ms).collect(),
        ) {
            Some(ms) => ms,
            None => {
                missing("round_ms");
                anyhow::bail!("no round_ms (config absent and < 2 finalized rounds)");
            }
        },
    };

    let current_round = rounds.iter().find(|r| r.round == vault.round);
    let current_expiry_ms = current_round
        .and_then(|r| r.expiry_ms)
        .map(|e| e as i64)
        .unwrap_or_else(|| now_ms() + round_ms as i64);

    // Strike actually sold this round, in USD-cross units (settlement per
    // underlying) so it's comparable to `spot`. On-chain `strike` is a scaled
    // ratio — settle-smallest per under-smallest = strike / 10^strike_scale —
    // so the whole-unit price is that × 10^(under_dec − settle_dec).
    let current_strike = current_round.and_then(|r| match (r.strike, r.strike_scale) {
        (Some(raw), Some(scale)) => {
            let exp = u_tok.decimals as i32 - s_tok.decimals as i32 - scale as i32;
            Some(raw as f64 * 10f64.powi(exp))
        }
        _ => None,
    });

    let aum_underlying = aum_underlying(vault, u_tok.decimals);

    // Tier 1: sum this round's RFQ premiums (settled → net, open → best bid).
    let (premium_underlying, confidence) =
        current_round_premium(&p.indexer, vault, s_tok.decimals, spot).await?;

    let inputs = VaultInputs {
        spot,
        sigma,
        aum_underlying,
        round_ms,
        current_expiry_ms,
        current_premium_underlying: premium_underlying,
        current_premium_confidence: confidence,
        current_strike,
        perf_fee: perf_bps as f64 / 10_000.0,
        mgmt_fee_annual: mgmt_bps as f64 / 10_000.0,
        horizon: p.model.forecast_horizon,
        delta_target: p.model.delta_target,
        apy_cap: p.model.apy_cap,
        assumed_vrp: p.model.assumed_vrp,
        vrp_band: p.model.vrp_band,
        vol_band: p.model.vol_band,
    };
    Ok(compute::predict(&inputs))
}

fn token_for<'a>(
    snapshot: &'a Snapshot,
    coin_type: &str,
    leg: &'static str,
) -> Result<&'a SupportedToken> {
    snapshot.token_by_coin_type(coin_type).ok_or_else(|| {
        missing("token");
        anyhow::anyhow!("no token-info catalog entry for {leg} {coin_type}")
    })
}

/// AUM in whole underlying units: held shares valued at pps, plus queued
/// deposits. `held_raw = total_shares × pps / PPS_SCALE`.
fn aum_underlying(vault: &Vault, decimals: u8) -> f64 {
    let held_raw = match vault.latest_pps {
        Some(pps) => ((vault.total_shares as u128).saturating_mul(pps) / PPS_SCALE) as u64,
        None => 0,
    };
    let aum_raw = held_raw.saturating_add(vault.pending_deposits);
    aum_raw as f64 / 10f64.powi(decimals as i32)
}

/// Sum the current round's RFQ premiums and convert to underlying. Returns
/// `(premium_underlying, confidence)` where confidence is the settled fraction
/// of round notional.
async fn current_round_premium(
    indexer: &IndexerClient,
    vault: &Vault,
    settlement_decimals: u8,
    spot: f64,
) -> Result<(f64, f64)> {
    let Some(bucket) = vault.current_bucket else {
        return Ok((0.0, 0.0));
    };
    let rfqs = indexer
        .rfqs(None, Some(vault.vault_id))
        .await
        .context("listing vault rfqs")?;

    let mut premium_raw: u64 = 0;
    let mut total_notional: u64 = 0;
    let mut settled_notional: u64 = 0;
    for r in rfqs.iter().filter(|r| r.bucket_id == Some(bucket)) {
        total_notional = total_notional.saturating_add(r.amount);
        match r.status.as_str() {
            "settled" => {
                settled_notional = settled_notional.saturating_add(r.amount);
                premium_raw = premium_raw.saturating_add(r.net_premium.unwrap_or(0));
            }
            "open" => premium_raw = premium_raw.saturating_add(r.best_premium.unwrap_or(0)),
            _ => {}
        }
    }

    let premium_settlement = premium_raw as f64 / 10f64.powi(settlement_decimals as i32);
    let premium_underlying = if spot > 0.0 { premium_settlement / spot } else { 0.0 };
    let confidence = if total_notional > 0 {
        settled_notional as f64 / total_notional as f64
    } else {
        0.0
    };
    Ok((premium_underlying, confidence))
}

fn missing(reason: &'static str) {
    metrics::counter!("price_charting_apy_inputs_missing_total", "reason" => reason).increment(1);
}

fn ms_to_dt(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
