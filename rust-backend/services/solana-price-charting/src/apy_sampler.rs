//! Vault-APY sampler — the genuinely live part of this service. Ported from
//! the Sui twin with the sources swapped:
//!
//!   - vaults / rounds / realized series ← solana-indexer GraphQL
//!     (`crates/solana-indexer-graphql`).
//!   - Tier-1 premium evidence ← the venue's **covered-call auctions** for
//!     the vault's current bucket (see [`current_round_premium`]), replacing
//!     the Sui RFQ premiums.
//!   - spot + realized vol ← oracle-client against solana-oracle-service.
//!   - Pure math (`apy::compute`) unchanged.
//!
//! Every `tick_interval`, for each covered-call vault:
//!   - **Predicted** (active vaults only) — Tier 1 annualizes the premium the
//!     vault is on track to collect this round from its live auctions;
//!     Tier 2 prices the next K rounds with Black–Scholes at the keeper's
//!     delta-target strike, net of fees. Each point is APPENDED to
//!     `vault_predicted_apy`, so the prediction history is retained.
//!   - **Realized** (every vault) — the indexer's `vaultApy` series (annualized
//!     pps growth per finalized round) is mirrored into `vault_realized_apy`,
//!     idempotent on (vault, round). The indexer stays the source of the
//!     formula; we persist it so it becomes a queryable time-series.
//!
//! There is no cursor: predictions are point-in-time recomputes and realized
//! rounds are idempotent, so history simply starts when sampling starts.
//! Per-vault failures are logged and skipped; only a failed vault *list* or
//! DB write fails the tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use tracing::{error, info, warn};

use solana_indexer_graphql::{Auction, IndexerClient, Vault};
use solana_token_info_client::{Snapshot, SupportedToken};

use crate::apy::compute::{self, PredictionPoint, VaultInputs};
use crate::apy::spot;
use crate::config::{ModelConfig, PythConfig};
use crate::db::models::{PredictedApyRow, RealizedApyRow};
use crate::state::AppState;

/// `options_math::PPS_SCALE` — pps is a 1e12-scaled underlying-per-share
/// (same fixed point as the Sui vault's).
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
                metrics::counter!("solana_price_charting_apy_tick_total", "outcome" => "ok")
                    .increment(1);
                metrics::gauge!("solana_price_charting_apy_last_success_timestamp_seconds")
                    .set(now_secs() as f64);
                info!(predicted, realized, "apy tick complete");
            }
            Err(e) => {
                consecutive_failures += 1;
                metrics::counter!("solana_price_charting_apy_tick_total", "outcome" => "error")
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
        metrics::histogram!("solana_price_charting_apy_tick_duration_seconds")
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
        match p.indexer.vault_apy(&vault.vault_id).await {
            Ok(points) => {
                for pt in points {
                    realized_rows.push(RealizedApyRow {
                        time: ms_to_dt(pt.t_ms as i64),
                        vault_id: vault.vault_id.clone(),
                        round: pt.round as i64,
                        apy: pt.apy,
                    });
                }
            }
            Err(e) => warn!(
                vault = %vault.vault_id,
                error = %format!("{e:#}"),
                "realized apy fetch skipped"
            ),
        }
    }

    // ── Predicted: active vaults only — a paused (decommissioned) vault
    // shouldn't get forward-looking APY. ──
    let active: Vec<&Vault> = vaults.iter().filter(|v| !v.deposits_paused).collect();

    // Resolve realized vol once for the whole tick: collect the distinct
    // (beta) feed ids across every active vault, then ask the oracle for
    // their sigmas in one shared bulk pass (it maps beta→stable, caches,
    // paces).
    let mut feeds: Vec<oracle_client::PriceFeedId> = Vec::new();
    for vault in &active {
        if let Some(u_tok) = p.snapshot.token_by_mint(&vault.underlying_mint) {
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
                metrics::counter!("solana_price_charting_apy_vault_compute_total", "outcome" => "ok")
                    .increment(1);
                for pt in &points {
                    metrics::gauge!(
                        "solana_price_charting_predicted_apy",
                        "vault" => vault.vault_id.clone(),
                        "kind" => pt.kind,
                    )
                    .set(pt.apy);
                    predicted_rows.push(PredictedApyRow {
                        time: now,
                        vault_id: vault.vault_id.clone(),
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
                metrics::counter!("solana_price_charting_apy_vault_compute_total", "outcome" => "error")
                    .increment(1);
                warn!(vault = %vault.vault_id, error = %format!("{e:#}"), "vault prediction skipped");
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
    metrics::counter!("solana_price_charting_predicted_apy_written_total")
        .increment(written.0 as u64);
    metrics::counter!("solana_price_charting_realized_apy_written_total")
        .increment(written.1 as u64);
    Ok(written)
}

/// Resolve market data + vault state and compute the predicted points for one
/// vault. Missing inputs are counted and surfaced as errors (skip the vault).
async fn compute_vault(
    p: &ApySamplerParams,
    vault: &Vault,
    sigmas: &HashMap<oracle_client::PriceFeedId, Result<f64>>,
) -> Result<Vec<PredictionPoint>> {
    let u_tok = token_for(&p.snapshot, &vault.underlying_mint, "underlying")?;
    let s_tok = token_for(&p.snapshot, &vault.settlement_mint, "settlement")?;

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
            warn!(vault = %vault.vault_id, error = %err, "vol unavailable; Tier 2 forecast skipped");
            0.0
        }
    };

    // Fees come from the vault's served on-chain config — never guessed. A
    // vault whose config hasn't been indexed yet is skipped (not faked).
    let (Some(perf_bps), Some(mgmt_bps)) = (vault.perf_fee_bps, vault.mgmt_fee_bps_annual) else {
        missing("config");
        anyhow::bail!("vault config not indexed yet (no fees served)");
    };

    let rounds = p.indexer.vault_rounds(&vault.vault_id).await.context("vault rounds")?;
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
    // ratio — settle-smallest per under-smallest = strike / 10^strike_scale
    // (options_math::apply_strike, same fixed point as the Sui bucket) — so
    // the whole-unit price is that × 10^(under_dec − settle_dec).
    let current_strike = current_round.and_then(|r| {
        strike_cross(r.strike, r.strike_scale, u_tok.decimals, s_tok.decimals)
    });

    let aum_underlying = aum_underlying(vault, u_tok.decimals);

    // Tier 1: sum this round's auction premiums (settled → net, open → best bid).
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
    mint: &str,
    leg: &'static str,
) -> Result<&'a SupportedToken> {
    snapshot.token_by_mint(mint).ok_or_else(|| {
        missing("token");
        anyhow::anyhow!("no solana-token-info catalog entry for {leg} mint {mint}")
    })
}

/// The round's sold strike as a whole-unit USD cross (settlement per 1
/// underlying), or `None` if the round's selection isn't indexed yet.
fn strike_cross(
    strike: Option<u128>,
    strike_scale: Option<u8>,
    underlying_decimals: u8,
    settlement_decimals: u8,
) -> Option<f64> {
    match (strike, strike_scale) {
        (Some(raw), Some(scale)) => {
            let exp = underlying_decimals as i32 - settlement_decimals as i32 - scale as i32;
            Some(raw as f64 * 10f64.powi(exp))
        }
        _ => None,
    }
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

/// Sum the current round's auction premiums and convert to underlying.
/// Returns `(premium_underlying, confidence)` where confidence is the
/// settled fraction of the round's auctioned notional.
///
/// This is the Solana analog of the Sui sampler's RFQ premium sum, with the
/// venue's covered-call auctions as the evidence:
///   - Sui `rfqs(vault)` filtered to the current bucket → here the indexer
///     filters server-side: `auctions(mode: covered_call, bucketId: <current
///     bucket>, creator: <vault>)`. The vault PDA is the creator of its own
///     round auctions (keeper flow), and the bucket is per-round, so this is
///     the same population. We fetch ALL statuses (not just `open`) so the
///     settled premium and the settled-fraction confidence carry over from
///     the Sui logic — `status: open` alone would drop both.
///   - Sui settled RFQ `net_premium` → settled auction `net_proceeds`
///     (gross winning bid minus the protocol fee — what the vault receives).
///   - Sui open RFQ `best_premium` → open auction `best_bid` (gross of the
///     settle-time fee; the same open-side asymmetry the Sui sampler had).
///   - `unsold` auctions contribute notional but no premium, like Sui's
///     cancelled/expired RFQs.
///   - Bids are denominated in the auction's `bid_mint`, which for a vault's
///     covered-call auctions is the vault's settlement mint, so the
///     settlement-decimals → spot conversion is unchanged.
async fn current_round_premium(
    indexer: &IndexerClient,
    vault: &Vault,
    settlement_decimals: u8,
    spot: f64,
) -> Result<(f64, f64)> {
    let Some(bucket) = vault.current_bucket.as_deref() else {
        return Ok((0.0, 0.0));
    };
    let auctions = indexer
        .auctions(None, Some("covered_call"), Some(bucket), Some(&vault.vault_id))
        .await
        .context("listing vault auctions")?;
    Ok(premium_from_auctions(&auctions, settlement_decimals, spot))
}

/// Pure aggregation over one round's auctions (unit-tested): premium in whole
/// underlying units + settled-notional confidence. Notional weighting uses the
/// auction's escrowed option-token `amount` — the analog of the Sui RFQ's
/// `amount`.
fn premium_from_auctions(
    auctions: &[Auction],
    settlement_decimals: u8,
    spot: f64,
) -> (f64, f64) {
    let mut premium_raw: u64 = 0;
    let mut total_notional: u64 = 0;
    let mut settled_notional: u64 = 0;
    for a in auctions {
        total_notional = total_notional.saturating_add(a.amount);
        match a.status.as_str() {
            "settled" => {
                settled_notional = settled_notional.saturating_add(a.amount);
                premium_raw = premium_raw.saturating_add(a.net_proceeds.unwrap_or(0));
            }
            "open" => premium_raw = premium_raw.saturating_add(a.best_bid.unwrap_or(0)),
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
    (premium_underlying, confidence)
}

fn missing(reason: &'static str) {
    metrics::counter!("solana_price_charting_apy_inputs_missing_total", "reason" => reason)
        .increment(1);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A covered-call auction fixture; tests override the fields they exercise.
    fn auction(status: &str, amount: u64) -> Auction {
        Auction {
            auction_id: "auc".into(),
            mode: "covered_call".into(),
            bucket_id: Some("bkt".into()),
            creator: "vault".into(),
            escrow_mint: "opt".into(),
            bid_mint: "usdc".into(),
            amount,
            notional: 0,
            reserve_bid: 0,
            deadline_ms: 0,
            max_deadline_ms: 0,
            min_increment_bps: 0,
            settle_authority: None,
            best_bid: None,
            best_bidder: None,
            status: status.into(),
            winner: None,
            token_recipient: None,
            position_id: None,
            gross_bid: None,
            fee: None,
            net_proceeds: None,
            bid_refunded: None,
        }
    }

    #[test]
    fn premium_sums_settled_net_and_open_best_bid() {
        // settled: net_proceeds counts (gross/fee do not); open: best_bid;
        // unsold: notional only. 6-decimal settlement, spot 2.0 settle/under:
        //   raw = 3_000_000 + 1_000_000 = 4 settle units → 2 underlying.
        let mut settled = auction("settled", 100);
        settled.gross_bid = Some(3_200_000);
        settled.fee = Some(200_000);
        settled.net_proceeds = Some(3_000_000);
        let mut open = auction("open", 100);
        open.best_bid = Some(1_000_000);
        let unsold = auction("unsold", 200);

        let (premium, confidence) =
            premium_from_auctions(&[settled, open, unsold], 6, 2.0);
        assert!((premium - 2.0).abs() < 1e-12, "premium {premium}");
        // settled 100 of 400 total notional.
        assert!((confidence - 0.25).abs() < 1e-12, "confidence {confidence}");
    }

    #[test]
    fn premium_handles_missing_bids_and_zero_spot() {
        // Open auction with no bid yet contributes nothing; zero/negative
        // spot can't convert, so premium is 0 rather than inf.
        let open = auction("open", 50);
        let (premium, confidence) = premium_from_auctions(&[open.clone()], 6, 2.0);
        assert_eq!(premium, 0.0);
        assert_eq!(confidence, 0.0);

        let mut settled = auction("settled", 50);
        settled.net_proceeds = Some(1_000_000);
        let (premium, _) = premium_from_auctions(&[settled], 6, 0.0);
        assert_eq!(premium, 0.0);

        // No auctions at all → (0, 0), same as Sui's no-RFQs case.
        assert_eq!(premium_from_auctions(&[], 6, 2.0), (0.0, 0.0));
    }

    #[test]
    fn strike_cross_matches_manual_conversion() {
        // BTC-like underlying (8 dec) vs USDC-like settlement (6 dec) at a
        // $65,000 strike. The on-chain ratio is settle-smallest per
        // under-smallest = 65_000 × 10^(6−8) = 650; at scale 6 the raw strike
        // is 650 × 1e6 = 650_000_000. The formula recovers the whole-unit
        // cross: raw × 10^(8 − 6 − 6) = 650e6 × 1e-4 = 65_000.
        let k = strike_cross(Some(650_000_000), Some(6), 8, 6).unwrap();
        assert!((k - 65_000.0).abs() < 1e-9);
        // Same-decimals pair: strike/10^scale is already the cross.
        let k = strike_cross(Some(2_000_000), Some(6), 6, 6).unwrap();
        assert!((k - 2.0).abs() < 1e-12);
        // Unindexed round → None.
        assert_eq!(strike_cross(None, Some(6), 6, 6), None);
        assert_eq!(strike_cross(Some(1), None, 6, 6), None);
    }

    #[test]
    fn aum_values_shares_at_pps_plus_pending() {
        let vault = Vault {
            vault_id: "v".into(),
            underlying_mint: "u".into(),
            settlement_mint: "s".into(),
            share_mint: "sh".into(),
            round: 1,
            current_bucket: None,
            // pps 1.5× on 1_000 shares (6-dec underlying) + 500_000 pending
            // = 1_500_000 + 500_000 raw = 2.0 whole units.
            latest_pps: Some(PPS_SCALE + PPS_SCALE / 2),
            total_shares: 1_000_000,
            pending_deposits: 500_000,
            deposits_paused: false,
            mgmt_fee_bps_annual: None,
            perf_fee_bps: None,
            round_ms: None,
            selling_window_ms: None,
            min_strike_bps_over_spot: None,
            max_strike_bps_over_spot: None,
        };
        assert!((aum_underlying(&vault, 6) - 2.0).abs() < 1e-12);
        // No pps yet → only pending deposits count.
        let mut fresh = vault;
        fresh.latest_pps = None;
        assert!((aum_underlying(&fresh, 6) - 0.5).abs() < 1e-12);
    }
}
