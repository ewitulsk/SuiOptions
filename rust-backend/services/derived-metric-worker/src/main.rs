use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};

use derived_metric_worker::compute::{self, PredictionPoint, VaultInputs};
use derived_metric_worker::db::models::NewPrediction;
use derived_metric_worker::db::{self, DbPool};
use derived_metric_worker::spot;
use derived_metric_worker::{Cli, Config};

use indexer_graphql::{IndexerClient, Vault};
use token_info_client::{Snapshot, SupportedToken, TokenInfoClient};

/// `vault.move::PPS_SCALE` — pps is a 1e12-scaled underlying-per-share.
const PPS_SCALE: u128 = 1_000_000_000_000;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("derived-metric-worker");

    let cli = Cli::parse();
    let cfg_path = cli.config.to_string_lossy().into_owned();
    info!(cfg_path, "loading config");
    let cfg = Config::load(&cfg_path).with_context(|| format!("loading config from {cfg_path}"))?;

    observability::ops::spawn(cfg.ops_addr);

    // One-shot alert-pipeline self-test: `ALERT_TEST=1` fires the canonical
    // test alert_id so the Loki → Grafana rule can be verified end-to-end.
    if std::env::var("ALERT_TEST").as_deref() == Ok("1") {
        tracing::error!(alert_id = "this-is-a-test-alert", "test alert emitted (ALERT_TEST=1)");
    }

    let pool = Arc::new(
        db::establish_pool(&cfg.database_url, 4)
            .context("connecting derived-metrics DB (required) — refusing to start without it")?,
    );
    db::run_migrations(&pool).context("running derived-metrics migrations")?;
    info!("derived-metrics DB connected and migrations applied");

    derived_metric_worker::read_api::spawn(cfg.read_addr, pool.clone());

    let http = reqwest::Client::new();
    // Shared, cached, paced realized-vol client. One instance for the whole
    // process so its (feed, day) cache and request pacer span every vault and
    // every tick — the Benchmarks closes it serves are immutable.
    let benchmark_vol = pyth_client::BenchmarkVol::new(http.clone(), cfg.pyth.benchmarks_url.clone());
    let indexer = IndexerClient::new(cfg.indexer_graphql_url.clone());
    let snapshot = TokenInfoClient::new(cfg.token_info_url.clone())
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .context("fetching token catalog from token-info")?;

    info!(
        environment = %cfg.environment,
        tick_secs = cfg.tick_secs,
        horizon = cfg.model.forecast_horizon,
        "derived-metric-worker starting"
    );

    let mut consecutive_failures: u32 = 0;
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.tick_secs.max(1)));
    loop {
        tick.tick().await;
        let started = Instant::now();
        match tick_once(&cfg, &http, &benchmark_vol, &indexer, &snapshot, &pool).await {
            Ok(written) => {
                consecutive_failures = 0;
                metrics::counter!("dmw_tick_total", "outcome" => "ok").increment(1);
                metrics::gauge!("dmw_last_success_timestamp_seconds").set(now_secs() as f64);
                info!(written, "tick complete");
            }
            Err(e) => {
                consecutive_failures += 1;
                metrics::counter!("dmw_tick_total", "outcome" => "error").increment(1);
                // Page only after repeated failures so a transient indexer/feed
                // blip doesn't wake anyone; the rule dedups over its window.
                if consecutive_failures >= 3 {
                    error!(
                        alert_id = "dmw-tick-failing",
                        consecutive_failures,
                        error = %e,
                        "derived-metric-worker tick failing repeatedly"
                    );
                } else {
                    warn!(error = %e, consecutive_failures, "tick errored");
                }
            }
        }
        metrics::histogram!("dmw_tick_duration_seconds").record(started.elapsed().as_secs_f64());
    }
}

/// One recompute pass over every vault. Per-vault failures are logged and
/// skipped (don't fail the whole tick); only a failed vault *list* or DB write
/// bubbles up. Returns the number of points written.
async fn tick_once(
    cfg: &Config,
    http: &reqwest::Client,
    benchmark_vol: &pyth_client::BenchmarkVol,
    indexer: &IndexerClient,
    snapshot: &Snapshot,
    pool: &Arc<DbPool>,
) -> Result<usize> {
    let vaults = indexer.vaults().await.context("listing vaults")?;
    let mut rows: Vec<NewPrediction> = Vec::new();

    // Resolve realized vol once for the whole tick: collect the distinct
    // stable benchmark feeds across every vault, then compute their sigmas in
    // a single shared bulk pass (cached + paced). Vaults sharing an underlying
    // resolve to one feed, and immutable closes are reused across ticks.
    let mut feeds: Vec<pyth_client::PriceFeedId> = Vec::new();
    for vault in &vaults {
        if let Some(u_tok) = snapshot.token_by_coin_type(vault.underlying_type.as_str()) {
            if let Ok(feed) = spot::benchmark_feed(u_tok) {
                if !feeds.contains(&feed) {
                    feeds.push(feed);
                }
            }
        }
    }
    let sigmas = benchmark_vol
        .realized_sigma_bulk(&feeds, cfg.pyth.vol_window_days, now_secs())
        .await;

    for vault in &vaults {
        match compute_vault(cfg, http, indexer, snapshot, vault, &sigmas).await {
            Ok(points) => {
                metrics::counter!("dmw_vault_compute_total", "outcome" => "ok").increment(1);
                let vid = vault.vault_id.to_hex();
                let now = chrono::Utc::now();
                for p in &points {
                    metrics::gauge!(
                        "dmw_predicted_apy",
                        "vault" => vid.clone(),
                        "kind" => p.kind,
                    )
                    .set(p.apy);
                    rows.push(NewPrediction {
                        vault_id: vid.clone(),
                        kind: p.kind.to_string(),
                        horizon: p.horizon,
                        t_ms: p.t_ms,
                        apy: p.apy,
                        confidence: p.confidence,
                        computed_at: now,
                    });
                }
            }
            Err(e) => {
                metrics::counter!("dmw_vault_compute_total", "outcome" => "error").increment(1);
                warn!(vault = %vault.vault_id.to_hex(), error = %e, "vault prediction skipped");
            }
        }
    }

    // Persist on the blocking pool. A DB write failure is page-worthy: the read
    // API will serve stale predictions until it recovers.
    let written = rows.len();
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || db::upsert_predictions(&pool, &rows))
        .await
        .context("join upsert task")?
        .map_err(|e| {
            error!(alert_id = "dmw-db-write-failed", error = %e, "failed to persist predictions");
            e
        })?;
    metrics::counter!("dmw_predictions_written_total").increment(written as u64);
    Ok(written)
}

/// Resolve market data + vault state and compute the predicted points for one
/// vault. Missing inputs are counted and surfaced as errors (skip the vault).
async fn compute_vault(
    cfg: &Config,
    http: &reqwest::Client,
    indexer: &IndexerClient,
    snapshot: &Snapshot,
    vault: &Vault,
    sigmas: &HashMap<pyth_client::PriceFeedId, Result<f64>>,
) -> Result<Vec<PredictionPoint>> {
    let u_tok = token_for(snapshot, vault.underlying_type.as_str(), "underlying")?;
    let s_tok = token_for(snapshot, vault.settlement_type.as_str(), "settlement")?;

    // Spot is required for both tiers (premium → underlying conversion).
    let spot = spot::resolve_cross(
        http,
        &cfg.pyth.hermes_url,
        u_tok,
        s_tok,
        cfg.pyth.max_publish_lag_ms,
        cfg.pyth.max_conf_bps,
    )
    .await
    .inspect_err(|_| missing("spot"))
    .context("resolving spot cross")?;

    // Vol only gates Tier 2; a vol failure still yields the Tier-1 point. Sigma
    // was resolved for the whole tick (bulk + cached); look this vault's feed up.
    let sigma = match spot::benchmark_feed(u_tok).ok().and_then(|f| sigmas.get(&f)) {
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

    let rounds = indexer.vault_rounds(vault.vault_id).await.context("vault rounds")?;
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

    let current_expiry_ms = rounds
        .iter()
        .find(|r| r.round == vault.round)
        .and_then(|r| r.expiry_ms)
        .map(|e| e as i64)
        .unwrap_or_else(|| now_ms() + round_ms as i64);

    let aum_underlying = aum_underlying(vault, u_tok.decimals);

    // Tier 1: sum this round's RFQ premiums (settled → net, open → best bid).
    let (premium_underlying, confidence) =
        current_round_premium(indexer, vault, s_tok.decimals, spot).await?;

    let inputs = VaultInputs {
        spot,
        sigma,
        aum_underlying,
        round_ms,
        current_expiry_ms,
        current_premium_underlying: premium_underlying,
        current_premium_confidence: confidence,
        perf_fee: perf_bps as f64 / 10_000.0,
        mgmt_fee_annual: mgmt_bps as f64 / 10_000.0,
        horizon: cfg.model.forecast_horizon,
        delta_target: cfg.model.delta_target,
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
    for r in rfqs.iter().filter(|r| r.bucket_id == bucket) {
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
    metrics::counter!("dmw_inputs_missing_total", "reason" => reason).increment(1);
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
