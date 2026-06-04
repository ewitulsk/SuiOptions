//! option-scheduler — bucket-creation lifecycle bot.
//!
//! Boot:
//!   1. Parse Cli + fetch the token-info snapshot + load secrets.
//!   2. Connect SuiClient; assert signer == deployer (only address with AdminCap).
//!   3. For each configured pair: resolve underlying/settlement `TokenInfo`,
//!      build a canonicalised PairKey.
//!   4. Open the scheduler DB (MANDATORY) and run migrations. If the DB is
//!      unreachable the binary fails fast and exits — there is no
//!      in-memory fallback.
//!   5. Spawn the indexer subscriber. It follows the live stream forever
//!      and is used ONLY to confirm submitted rolls and drive the
//!      reconciler — never to make a roll decision.
//!
//! Tick loop (every `tick_secs`, default 60):
//!   For each pair, read the latest active expiry FROM THE DB. If
//!   `latest_expiry - now < roll_threshold_ms`, compute the next expiry
//!   via the cadence, claim the slot in the DB (the partial UNIQUE index
//!   is the hard dedup), resolve the strike grid via current spot, and
//!   submit `bucket::new_call_option<U, S>` (or just log under --dry-run).
//!
//! The DB is the single source of truth for which (pair, expiry) slots
//! have been rolled. The scheduler never relies on indexer-flowed state to
//! decide whether to roll, so a stale or empty registry — e.g. right after
//! a restart — can no longer cause a duplicate family.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use token_info_client::TokenInfoClient;

use option_scheduler::config::{PairConfig, SchedulerConfig};
use option_scheduler::db;
use option_scheduler::families::{CanonicalType, PairKey, Registry, log_registry, run_subscriber};
use option_scheduler::roller::{self, ErrorClass, RollPlan};
use option_scheduler::schedule::{decide_tick, SkipReason, TickDecision};
use option_scheduler::spot::ResolvedSpotSource;
use option_scheduler::strike_grid::build_strike_grid_for_pair;
use option_scheduler::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = SchedulerConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    runtime_config::health::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    // Fetch the protocol ids + supported-token catalog from token-info.
    // Hard cutover: if token-info is unreachable after the retry window we
    // crash (no deployments.json fallback).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!("fetching catalog from token-info at {}", cli.token_info_url)
        })?;

    let package = snapshot.package()?;
    let admin_cap = snapshot.admin_cap()?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;

    // AdminCap belongs to the deployer only — exchange enforces the same check
    // (tools/exchange/src/main.rs). A scheduler signed by anyone else is
    // useless and we'd rather fail loudly at boot than hit a chain revert on
    // every tick.
    let deployer = snapshot.deployer_address()?;
    if wrap.signer.address != deployer {
        return Err(anyhow!(
            "configured signer {} ≠ deployer {} from token-info — \
             only the deployer holds AdminCap",
            wrap.signer.address,
            deployer
        ));
    }

    if cfg.pairs.is_empty() {
        return Err(anyhow!(
            "no [[pairs]] configured in {} — the scheduler would have nothing to do",
            cli.config.display()
        ));
    }

    // HTTP client shared by every Pyth lookup. Pyth's public Hermes
    // endpoint applies a 10-req-per-10-second cap per source IP, so a
    // single shared client is the right move regardless of pair count.
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;

    // Resolve every configured pair against deployments. Both the
    // testTokens entry (coin type, faucet, decimals) and the off-chain
    // token catalog (pyth feed) are consulted; Pyth pairs without a feed
    // id in deployments fail here, not at first tick.
    let mut pair_keys: Vec<PairKey> = Vec::with_capacity(cfg.pairs.len());
    let mut pair_meta: Vec<PairMeta> = Vec::with_capacity(cfg.pairs.len());
    for pair in &cfg.pairs {
        let u = snapshot.token(&pair.underlying).with_context(|| {
            format!(
                "underlying {} not in token-info testTokens",
                pair.underlying
            )
        })?;
        let s = snapshot.token(&pair.settlement).with_context(|| {
            format!(
                "settlement {} not in token-info testTokens",
                pair.settlement
            )
        })?;
        let u_spec = snapshot.token_spec(&pair.underlying).with_context(|| {
            format!(
                "underlying {} not in token-info catalog",
                pair.underlying
            )
        })?;
        let s_spec = snapshot.token_spec(&pair.settlement).with_context(|| {
            format!(
                "settlement {} not in token-info catalog",
                pair.settlement
            )
        })?;
        let spot = ResolvedSpotSource::from_config(&pair.spot, u_spec, s_spec)
            .with_context(|| {
                format!(
                    "resolving spot source for {}/{}",
                    pair.underlying, pair.settlement
                )
            })?;
        pair_keys.push(PairKey {
            underlying_symbol: pair.underlying.clone(),
            settlement_symbol: pair.settlement.clone(),
            underlying: CanonicalType::parse(&u.coin_type)?,
            settlement: CanonicalType::parse(&s.coin_type)?,
        });
        pair_meta.push(PairMeta {
            cfg: pair.clone(),
            underlying_type: u.coin_type.clone(),
            settlement_type: s.coin_type.clone(),
            underlying_decimals: u_spec.decimals,
            settlement_decimals: s_spec.decimals,
            spot,
        });
        info!(
            underlying = %pair.underlying,
            settlement = %pair.settlement,
            expiry_interval_ms = pair.expiry_interval_ms,
            spot = ?pair.spot,
            "pair configured"
        );
    }
    let pair_keys = Arc::new(pair_keys);
    let registry = Registry::new();

    // ── Scheduler DB (mandatory) ────────────────────────────────────
    // The DB is the single source of truth for which (pair, expiry)
    // slots have been rolled. Connect eagerly and fail hard/fast if it
    // is unreachable — there is no in-memory fallback, by design: a
    // scheduler that can't reach its DB must not roll, because the only
    // hard guard against duplicate bucket creation is the DB's partial
    // UNIQUE index.
    let db_pool: db::DbPool = db::establish_pool(&cfg.scheduler_database_url, 4)
        .context("connecting to scheduler DB (required) — refusing to start without it")?;
    db::run_migrations(&db_pool).context("running scheduler DB migrations")?;
    info!("scheduler DB connected and migrations applied");

    // Boot reconciliation: log active rows.
    match db::all_active_rows(&db_pool) {
        Ok(rows) => {
            for row in &rows {
                info!(
                    id = row.id,
                    pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
                    expiry_ms = row.expiry_ms,
                    state = %row.state,
                    tx_digest = ?row.tx_digest,
                    "active roll at boot"
                );
            }
            if rows.is_empty() {
                info!("no active rolls in scheduler DB at boot");
            }
        }
        Err(e) => warn!(error = %e, "failed to read active rolls at boot"),
    }

    // Indexer subscriber — runs forever in the background. It is used
    // ONLY for post-submit confirmation (marking rolls confirmed) and to
    // drive the reconciler; it never feeds the roll decision.
    {
        let url = cfg.indexer_url.clone();
        let registry = registry.clone();
        let pair_keys = pair_keys.clone();
        let pool = db_pool.clone();
        tokio::spawn(async move {
            if let Err(e) = run_subscriber(url, registry, pair_keys, Some(pool)).await {
                error!(error = %e, "indexer subscriber exited");
            }
        });
    }

    // Give the snapshot a moment to land before the first tick so the
    // dry-run print isn't empty. Anything we miss now will be filled in
    // on the next tick.
    sleep(Duration::from_secs(2)).await;
    log_registry(&registry, &pair_keys);

    // ── Reconciler task ─────────────────────────────────────────────
    {
        let pool = db_pool.clone();
        let registry = registry.clone();
        let safety_margin = cfg.reconciler_safety_margin;
        let interval = Duration::from_secs(cfg.reconciler_interval_secs.max(1));
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_reconciler(&pool, &registry, safety_margin) {
                    warn!(error = %e, "reconciler tick errored");
                }
                sleep(interval).await;
            }
        });
        info!(
            interval_secs = cfg.reconciler_interval_secs,
            safety_margin,
            "reconciler task started"
        );
    }

    let tick = Duration::from_secs(cfg.tick_secs.max(1));
    info!(
        tick_secs = cfg.tick_secs,
        roll_threshold_ms = cfg.roll_threshold_ms,
        dry_run = cli.dry_run,
        "tick loop starting"
    );

    loop {
        if let Err(e) = tick_once(
            &cli,
            &cfg,
            &registry,
            &pair_meta,
            &http_client,
            &wrap,
            package,
            admin_cap,
            &db_pool,
        )
        .await
        {
            warn!(error = %e, "tick errored");
        }
        sleep(tick).await;
    }
}

struct PairMeta {
    cfg: PairConfig,
    underlying_type: String,
    settlement_type: String,
    /// Cached at boot so the tick loop can compute chain-unit spots
    /// against any `strike_scale` the planner picks (post-SO-55 the spot
    /// source returns a USD cross, not pre-scaled chain units).
    underlying_decimals: u8,
    settlement_decimals: u8,
    spot: ResolvedSpotSource,
}

async fn tick_once(
    cli: &Cli,
    cfg: &SchedulerConfig,
    registry: &Registry,
    pairs: &[PairMeta],
    http_client: &reqwest::Client,
    wrap: &SuiClientWrapper,
    package: sui_types::base_types::ObjectID,
    admin_cap: sui_types::base_types::ObjectID,
    db_pool: &db::DbPool,
) -> Result<()> {
    let now = now_ms();
    for meta in pairs {
        let pair_label = format!("{}/{}", meta.cfg.underlying, meta.cfg.settlement);

        // The scheduler DB is the SOLE authority for what has been rolled.
        // The indexer-fed registry is deliberately not consulted for the
        // roll decision — relying on indexer-flowed state is exactly what
        // produced duplicate families. A transient DB read error skips the
        // pair this tick (retried next tick) rather than risking a roll on
        // missing state.
        let latest_expiry = match db::latest_active_expiry(
            db_pool,
            &meta.cfg.underlying,
            &meta.cfg.settlement,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "latest_active_expiry failed; skipping pair this tick");
                continue;
            }
        };

        let decision = decide_tick(
            latest_expiry,
            now,
            cfg.roll_threshold_ms,
            meta.cfg.expiry_interval_ms,
        );
        debug!(
            pair = %pair_label,
            latest_expiry = ?latest_expiry,
            decision = ?decision,
            "tick: evaluating pair"
        );
        let next_expiry = match decision {
            TickDecision::Roll { next_expiry_ms } => next_expiry_ms,
            TickDecision::Skip(SkipReason::BeyondRollWindow { .. }) => continue,
            TickDecision::Skip(SkipReason::AlreadyAtComputedExpiry) => {
                warn!(
                    pair = %pair_label,
                    latest_expiry = ?latest_expiry,
                    "skipping roll: cadence picker returned an expiry the chain already has"
                );
                continue;
            }
        };

        // Claim the slot. The partial UNIQUE index on
        // (underlying, settlement, expiry) is the hard guarantee: at most
        // one active row per slot, so a duplicate roll can never be
        // submitted even across restarts or concurrent ticks. The anchor
        // sequence lets the reconciler later tell whether the indexer has
        // caught up past this submit.
        let anchor_seq = registry.last_sequence();
        match db::claim_slot(
            db_pool,
            &meta.cfg.underlying,
            &meta.cfg.settlement,
            next_expiry,
            anchor_seq,
        ) {
            Ok(true) => {
                debug!(pair = %pair_label, expiry_ms = next_expiry, "slot claimed");
            }
            Ok(false) => {
                debug!(
                    pair = %pair_label,
                    expiry_ms = next_expiry,
                    "slot already claimed; skipping"
                );
                continue;
            }
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "claim_slot failed; skipping");
                continue;
            }
        }

        let spot_usd_cross = match meta
            .spot
            .resolve_usd_cross(http_client, &cfg.pyth.hermes_url)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "spot resolve failed; skipping");
                // Clean up the pending claim so the next tick can retry.
                let _ = db::delete_pending(
                    db_pool,
                    &meta.cfg.underlying,
                    &meta.cfg.settlement,
                    next_expiry,
                );
                continue;
            }
        };
        let grid = match build_strike_grid_for_pair(
            spot_usd_cross,
            meta.underlying_decimals,
            meta.settlement_decimals,
            meta.cfg.strikes_below,
            meta.cfg.strikes_above,
            meta.cfg.interval_pct,
        ) {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "strike grid invalid; skipping");
                let _ = db::delete_pending(
                    db_pool,
                    &meta.cfg.underlying,
                    &meta.cfg.settlement,
                    next_expiry,
                );
                continue;
            }
        };

        let plan = RollPlan {
            underlying_symbol: meta.cfg.underlying.clone(),
            settlement_symbol: meta.cfg.settlement.clone(),
            underlying_type: meta.underlying_type.clone(),
            settlement_type: meta.settlement_type.clone(),
            expiry_ms: next_expiry,
            grid,
        };
        plan.log_intent(cli.dry_run);

        if cli.dry_run {
            // Under dry-run, clean up the pending row since we won't submit.
            let _ = db::delete_pending(
                db_pool,
                &meta.cfg.underlying,
                &meta.cfg.settlement,
                next_expiry,
            );
            continue;
        }

        // ── Phase 2 step 3: submit + classify ──────────────────────
        match roller::submit(wrap, package, admin_cap, &plan, cli.gas_budget).await {
            Ok(out) => {
                info!(
                    digest = %out.digest,
                    bucket_count = out.bucket_ids.len(),
                    "rolled buckets submitted"
                );
                for id in &out.bucket_ids {
                    info!(bucket_id = %id, "new bucket");
                }
                let ids: Vec<String> = out.bucket_ids.iter().map(|id| id.to_string()).collect();
                if let Err(e) = db::mark_submitted(
                    db_pool,
                    &meta.cfg.underlying,
                    &meta.cfg.settlement,
                    next_expiry,
                    &out.digest,
                    &ids,
                ) {
                    warn!(error = %e, "mark_submitted failed");
                }
            }
            Err(e) => {
                let class = roller::classify_error(&e);
                warn!(
                    error = %e,
                    class = ?class,
                    pair = %pair_label,
                    "new_call_option submit failed"
                );
                match class {
                    ErrorClass::DefinitelyNotSent => {
                        let _ = db::delete_pending(
                            db_pool,
                            &meta.cfg.underlying,
                            &meta.cfg.settlement,
                            next_expiry,
                        );
                    }
                    ErrorClass::Ambiguous => {
                        let _ = db::mark_needs_reconciliation(
                            db_pool,
                            &meta.cfg.underlying,
                            &meta.cfg.settlement,
                            next_expiry,
                            &format!("{e:#}"),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Phase 4: reconciler — resolves `needs_reconciliation` rows once the
/// indexer has provably caught up past the submit point.
fn run_reconciler(
    pool: &db::DbPool,
    registry: &Registry,
    safety_margin: u64,
) -> Result<()> {
    let rows = db::needs_reconciliation_rows(pool)?;
    if rows.is_empty() {
        return Ok(());
    }
    let current_seq = registry.last_sequence();
    for row in rows {
        let anchor = row.submit_anchor_seq.unwrap_or(0) as u64;
        if current_seq <= anchor + safety_margin {
            debug!(
                id = row.id,
                current_seq,
                anchor,
                safety_margin,
                "reconciler: indexer hasn't caught up yet"
            );
            continue;
        }
        // Indexer has caught up. Check if a BucketCreated arrived for
        // this (pair, expiry) — if it did, Phase 3 already set
        // state='confirmed', so we only see rows that are still
        // needs_reconciliation here, meaning no bucket arrived.
        info!(
            id = row.id,
            pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
            expiry_ms = row.expiry_ms,
            "reconciler: indexer caught up with no matching BucketCreated — safe to clear"
        );
        db::delete_reconciled(pool, row.id)?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
