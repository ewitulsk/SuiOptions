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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::coin_pkg::PoolCreation;
use sui_tx::tx::deepbook::{derived_pool_params, DeepBookHandles};
use token_info_client::TokenInfoClient;

use indexer_graphql::IndexerClient;

use option_scheduler::config::{GridConfig, PairConfig, SchedulerConfig, VaultTemplate};
use option_scheduler::db;
use option_scheduler::families::{CanonicalType, PairKey};
use option_scheduler::roller::{self, ErrorClass, RollPlan};
use option_scheduler::schedule::{decide_tick, SkipReason, TickDecision};
use option_scheduler::spot::ResolvedSpotSource;
use option_scheduler::strike_grid::{build_strike_grid_for_pair, build_z_ladder_for_pair};
use option_scheduler::vault_roller::{self, VaultPairSpec};
use option_scheduler::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("option-scheduler");

    let cli = Cli::parse();
    let cfg = SchedulerConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    observability::ops::spawn(cfg.health_addr);
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

    // DeepBook pool creation (SO-171). The scheduler now creates the
    // permissionless pool for each call coin it mints, so the frontend no
    // longer has to. Optional: a network without a DeepBook deployment in
    // token-info still rolls buckets, just without pools. The deployer signer
    // holds DEEP and the creation fee recycles to it (registry treasury), so
    // pools cost only gas.
    let deepbook_cfg = match snapshot.deepbook() {
        Some(db) => {
            let pool_cfg = DeepBookPoolCfg {
                handles: DeepBookHandles {
                    package: db.package()?,
                    original_package: db.original_package()?,
                    registry: db.registry()?,
                },
                deep_coin_type: db.deep_coin_type.clone(),
                pool_creation_fee: db.pool_creation_fee_units()?,
            };
            info!(
                package = %pool_cfg.handles.package,
                registry = %pool_cfg.handles.registry,
                fee = pool_cfg.pool_creation_fee,
                "deepbook pool creation enabled"
            );
            Some(pool_cfg)
        }
        None => {
            warn!(
                "token-info reports no DeepBook deployment — rolling buckets without pools"
            );
            None
        }
    };

    // HTTP client shared by every Pyth lookup. Pyth's public Hermes
    // endpoint applies a 10-req-per-10-second cap per source IP, so a
    // single shared client is the right move regardless of pair count.
    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;

    // Resolve every configured pair against the /tokens catalog (coin type,
    // decimals, Pyth feed). The scheduler never mints, so it touches no
    // faucet/testTokens data. Pyth pairs without a feed id fail here, not at
    // first tick.
    let mut pair_keys: Vec<PairKey> = Vec::with_capacity(cfg.pairs.len());
    let mut pair_meta: Vec<PairMeta> = Vec::with_capacity(cfg.pairs.len());
    // Vault-eligible pairs: those with a Pyth feed on BOTH legs (a vault pins
    // feed ids for its oracle reads, so feedless test-token pairs get buckets
    // but no vault). Empty unless `[vault_template]` is configured.
    let mut vault_entries: Vec<VaultEntry> = Vec::new();
    for pair in &cfg.pairs {
        let u_spec = snapshot.token_spec(&pair.underlying).with_context(|| {
            format!("underlying {} not in token-info catalog", pair.underlying)
        })?;
        let s_spec = snapshot.token_spec(&pair.settlement).with_context(|| {
            format!("settlement {} not in token-info catalog", pair.settlement)
        })?;
        let spot = ResolvedSpotSource::from_config(&pair.spot, u_spec, s_spec)
            .with_context(|| {
                format!(
                    "resolving spot source for {}/{}",
                    pair.underlying, pair.settlement
                )
            })?;
        let pair_key = PairKey {
            underlying_symbol: pair.underlying.clone(),
            settlement_symbol: pair.settlement.clone(),
            underlying: CanonicalType::parse(&u_spec.coin_type)?,
            settlement: CanonicalType::parse(&s_spec.coin_type)?,
        };
        if cfg.vault_template.is_some() {
            match (u_spec.pyth_feed(), s_spec.pyth_feed()) {
                (Ok(u_feed), Ok(s_feed)) => vault_entries.push(VaultEntry {
                    key: pair_key.clone(),
                    spec: VaultPairSpec {
                        underlying_symbol: pair.underlying.clone(),
                        settlement_symbol: pair.settlement.clone(),
                        underlying_type: u_spec.coin_type.clone(),
                        settlement_type: s_spec.coin_type.clone(),
                        underlying_decimals: u_spec.decimals,
                        settlement_decimals: s_spec.decimals,
                        underlying_feed_id: u_feed.0.to_vec(),
                        settlement_feed_id: s_feed.0.to_vec(),
                    },
                }),
                _ => info!(
                    pair = %format!("{}/{}", pair.underlying, pair.settlement),
                    "no Pyth feed on one or both legs — skipping vault creation for this pair"
                ),
            }
        }
        pair_keys.push(pair_key);
        pair_meta.push(PairMeta {
            cfg: pair.clone(),
            underlying_type: u_spec.coin_type.clone(),
            settlement_type: s_spec.coin_type.clone(),
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

    // JIT indexer client. Used ONLY to confirm submitted rolls landed and to
    // read the high-water sequence the reconciler gates on — never to make a
    // roll decision.
    let indexer = Arc::new(IndexerClient::new(cfg.indexer_graphql_url.clone()));

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
    match db::all_active_vault_rows(&db_pool) {
        Ok(rows) => {
            for row in &rows {
                info!(
                    id = row.id,
                    pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
                    state = %row.state,
                    vault_id = ?row.vault_id,
                    "active vault at boot"
                );
            }
        }
        Err(e) => warn!(error = %e, "failed to read active vaults at boot"),
    }

    // ── Confirmation + reconciler task ──────────────────────────────
    // Periodically: (1) confirm submitted rolls whose bucket the indexer now
    // reports, and (2) clear ambiguous rows once the indexer has provably
    // caught up past their submit point. Both are JIT GraphQL queries — there
    // is no live stream. Neither feeds the roll decision.
    {
        let pool = db_pool.clone();
        let pair_keys = pair_keys.clone();
        let indexer = indexer.clone();
        let safety_margin = cfg.reconciler_safety_margin;
        let interval = Duration::from_secs(cfg.reconciler_interval_secs.max(1));
        tokio::spawn(async move {
            loop {
                metrics::counter!("scheduler_runs_total", "job" => "reconcile").increment(1);
                let started = std::time::Instant::now();
                if let Err(e) = run_reconciler(&pool, &indexer, &pair_keys, safety_margin).await {
                    warn!(error = %e, "reconciler tick errored");
                }
                metrics::histogram!("scheduler_job_duration_seconds", "job" => "reconcile")
                    .record(started.elapsed().as_secs_f64());
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
    let vault_interval = Duration::from_millis(cfg.vault_check_interval_ms.max(1));
    info!(
        tick_secs = cfg.tick_secs,
        roll_threshold_ms = cfg.roll_threshold_ms,
        vault_check_interval_ms = cfg.vault_check_interval_ms,
        vault_pairs = vault_entries.len(),
        dry_run = cli.dry_run,
        "tick loop starting"
    );

    // Run the first vault pass on the opening tick (None ⇒ due).
    let mut last_vault_check: Option<Instant> = None;
    loop {
        metrics::counter!("scheduler_runs_total", "job" => "roll").increment(1);
        let started = std::time::Instant::now();
        if let Err(e) = tick_once(
            &cli,
            &cfg,
            &indexer,
            &pair_meta,
            &http_client,
            &wrap,
            package,
            admin_cap,
            &db_pool,
            deepbook_cfg.as_ref(),
        )
        .await
        {
            warn!(error = %e, "tick errored");
        }
        metrics::histogram!("scheduler_job_duration_seconds", "job" => "roll")
            .record(started.elapsed().as_secs_f64());

        // Vault-ensure pass, gated to `vault_check_interval_ms`. Independent of
        // the bucket-roll cadence above. Disabled unless `[vault_template]` is
        // set and at least one pair is vault-eligible.
        if let Some(template) = cfg.vault_template.as_ref() {
            if !vault_entries.is_empty()
                && last_vault_check.is_none_or(|t| t.elapsed() >= vault_interval)
            {
                last_vault_check = Some(Instant::now());
                metrics::counter!("scheduler_runs_total", "job" => "vault").increment(1);
                let started = Instant::now();
                if let Err(e) = vault_pass(
                    &cli,
                    &indexer,
                    &vault_entries,
                    &wrap,
                    package,
                    admin_cap,
                    &db_pool,
                    template,
                )
                .await
                {
                    warn!(error = %format!("{e:#}"), "vault pass errored");
                }
                metrics::histogram!("scheduler_job_duration_seconds", "job" => "vault")
                    .record(started.elapsed().as_secs_f64());
            }
        }

        sleep(tick).await;
    }
}

/// Vault-ensure pass: query the indexer's vault set once, then for each
/// vault-eligible pair create a vault if one doesn't already exist. Per-pair
/// errors are logged and don't abort the pass.
#[allow(clippy::too_many_arguments)]
async fn vault_pass(
    cli: &Cli,
    indexer: &IndexerClient,
    entries: &[VaultEntry],
    wrap: &SuiClientWrapper,
    package: sui_types::base_types::ObjectID,
    admin_cap: sui_types::base_types::ObjectID,
    db_pool: &db::DbPool,
    template: &VaultTemplate,
) -> Result<()> {
    let vaults = indexer
        .vaults()
        .await
        .context("listing vaults for vault pass")?;
    for entry in entries {
        let existing = vaults
            .iter()
            .find(|v| {
                entry
                    .key
                    .matches_assets(&v.underlying_type, &v.settlement_type)
            })
            .map(|v| v.vault_id.to_hex());
        if let Err(e) = vault_roller::ensure_vault(
            wrap,
            package,
            admin_cap,
            db_pool,
            &entry.spec,
            template,
            existing,
            cli.dry_run,
            cli.gas_budget,
        )
        .await
        {
            warn!(
                pair = %format!("{}/{}", entry.spec.underlying_symbol, entry.spec.settlement_symbol),
                error = %format!("{e:#}"),
                "ensure_vault failed"
            );
        }
    }
    Ok(())
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

/// DeepBook pool-creation context, resolved from token-info at boot.
struct DeepBookPoolCfg {
    handles: DeepBookHandles,
    deep_coin_type: String,
    pool_creation_fee: u64,
}

/// A vault-eligible pair: the canonical key (to match the indexer's `vaults`
/// view) plus everything `ensure_vault` needs to create one.
struct VaultEntry {
    key: PairKey,
    spec: VaultPairSpec,
}

/// Resolve the per-roll strike set: the legacy percentage grid, or the
/// vol-aware z-ladder when `[pairs.grid]` is configured (doc 05 §4.2).
async fn resolve_strikes(
    pyth_cfg: &option_scheduler::config::PythGlobalConfig,
    meta: &PairMeta,
    http_client: &reqwest::Client,
    spot_usd_cross: f64,
    next_expiry_ms: u64,
) -> Result<(Vec<u128>, u8)> {
    let Some(GridConfig::ZLadder {
        ladder,
        vol_window_days,
        vol_floor,
        vol_ceiling,
        sigma_fallback,
    }) = &meta.cfg.grid
    else {
        let g = build_strike_grid_for_pair(
            spot_usd_cross,
            meta.underlying_decimals,
            meta.settlement_decimals,
            meta.cfg.strikes_below,
            meta.cfg.strikes_above,
            meta.cfg.interval_pct,
        )?;
        return Ok((g.strikes(), g.strike_scale));
    };

    let now_ms = now_ms() as i64;
    // σ: realized vol from Pyth Benchmarks for live pairs; the configured
    // fallback covers static (test-token) pairs and benchmark outages.
    let sigma = match &meta.spot {
        ResolvedSpotSource::Pyth { underlying_feed, .. } => {
            match option_scheduler::sigma::realized_sigma_from_benchmarks(
                http_client,
                &pyth_cfg.benchmarks_url,
                *underlying_feed,
                *vol_window_days,
                now_ms / 1000,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    let fallback = sigma_fallback.ok_or_else(|| {
                        anyhow!("realized vol fetch failed and no sigma_fallback: {e:#}")
                    })?;
                    warn!(error = %e, fallback, "realized vol fetch failed; using sigma_fallback");
                    fallback
                }
            }
        }
        ResolvedSpotSource::Static { .. } => sigma_fallback.ok_or_else(|| {
            anyhow!("z-ladder grid on a static-spot pair requires sigma_fallback")
        })?,
    };
    let sigma = sigma.clamp(*vol_floor, *vol_ceiling);

    let tau_ms = next_expiry_ms.saturating_sub(now_ms as u64);
    if tau_ms == 0 {
        return Err(anyhow!("next expiry {next_expiry_ms} is not in the future"));
    }
    let tau_years = tau_ms as f64 / (365.0 * 86_400_000.0);

    build_z_ladder_for_pair(
        spot_usd_cross,
        sigma,
        tau_years,
        ladder,
        meta.underlying_decimals,
        meta.settlement_decimals,
    )
}

async fn tick_once(
    cli: &Cli,
    cfg: &SchedulerConfig,
    indexer: &IndexerClient,
    pairs: &[PairMeta],
    http_client: &reqwest::Client,
    wrap: &SuiClientWrapper,
    package: sui_types::base_types::ObjectID,
    admin_cap: sui_types::base_types::ObjectID,
    db_pool: &db::DbPool,
    deepbook: Option<&DeepBookPoolCfg>,
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
        //
        // The anchor is read JIT from the indexer head. If that read fails we
        // skip the roll this tick (retried next tick) rather than stamp a
        // bogus anchor — an anchor that's too low could let the reconciler
        // prematurely conclude a real submit never landed.
        let anchor_seq = match indexer.head_sequence().await {
            Ok(seq) => seq,
            Err(e) => {
                warn!(
                    pair = %pair_label,
                    error = %e,
                    "skipping roll: could not read indexer head sequence for submit anchor"
                );
                continue;
            }
        };
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
        let (strikes, strike_scale) =
            match resolve_strikes(&cfg.pyth, meta, http_client, spot_usd_cross, next_expiry)
                .await
            {
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
            underlying_decimals: meta.underlying_decimals,
            settlement_decimals: meta.settlement_decimals,
            expiry_ms: next_expiry,
            strikes,
            strike_scale,
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

        // Per-roll DeepBook pool params: the grid is derived from the pair's
        // decimals (call coin inherits the underlying's), so buckets and pools
        // land in one atomic tx. `None` on a network without a DeepBook
        // deployment — the roll then creates buckets only.
        let pool_creation = deepbook.map(|db| {
            let (tick, lot, min) =
                derived_pool_params(meta.underlying_decimals, meta.settlement_decimals);
            PoolCreation {
                deepbook_package: db.handles.package,
                registry: db.handles.registry,
                deep_coin_type: db.deep_coin_type.clone(),
                fee: db.pool_creation_fee,
                tick,
                lot,
                min,
            }
        });

        // ── Phase 2 step 3: submit + classify ──────────────────────
        match roller::submit(
            wrap,
            package,
            admin_cap,
            &plan,
            pool_creation.as_ref(),
            cli.gas_budget,
        )
        .await
        {
            Ok(out) => {
                metrics::counter!("scheduler_tx_total", "job" => "roll", "outcome" => "ok")
                    .increment(1);
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
                metrics::counter!("scheduler_tx_total", "job" => "roll", "outcome" => "error")
                    .increment(1);
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

/// Confirmation + reconciliation pass (JIT).
///
/// Phase 3 (confirm): for every submitted / needs_reconciliation row, ask the
/// indexer whether a bucket for that (pair, expiry) now exists; if so, mark
/// the row confirmed. This replaces the old push-driven `BucketCreated`
/// confirmation.
///
/// Phase 4 (reconcile): any row still `needs_reconciliation` once the indexer
/// head has provably caught up past the submit anchor had no bucket land —
/// clear it.
async fn run_reconciler(
    pool: &db::DbPool,
    indexer: &IndexerClient,
    pair_keys: &[PairKey],
    safety_margin: u64,
) -> Result<()> {
    confirm_landed_rolls(pool, indexer, pair_keys).await?;

    let rows = db::needs_reconciliation_rows(pool)?;
    if rows.is_empty() {
        return Ok(());
    }
    let current_seq = indexer.head_sequence().await?;
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
        // Indexer has caught up. confirm_landed_rolls above already flipped
        // any row whose bucket exists to 'confirmed', so a row still in
        // needs_reconciliation here means no bucket ever landed.
        info!(
            id = row.id,
            pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
            expiry_ms = row.expiry_ms,
            "reconciler: indexer caught up with no matching bucket — safe to clear"
        );
        db::delete_reconciled(pool, row.id)?;
    }
    Ok(())
}

/// For each submitted / needs_reconciliation row, JIT-query the indexer for a
/// bucket at that expiry matching the row's configured pair, and confirm the
/// row if one exists. Idempotent: `confirm_from_indexer` only touches rows
/// still in a confirmable state.
async fn confirm_landed_rolls(
    pool: &db::DbPool,
    indexer: &IndexerClient,
    pair_keys: &[PairKey],
) -> Result<()> {
    let rows = db::all_active_rows(pool)?;
    for row in rows {
        if !matches!(row.state.as_str(), "submitted" | "needs_reconciliation") {
            continue;
        }
        let expiry = row.expiry_ms as u64;
        let buckets = indexer.buckets(false, None, None, Some(expiry)).await?;
        for b in buckets {
            // A bucket at this expiry could belong to a different configured
            // pair; confirm only against the pair that actually matches this
            // row's symbols.
            let Some(pk) = pair_keys
                .iter()
                .find(|p| p.matches_assets(&b.asset_type, &b.settlement_type))
            else {
                continue;
            };
            if pk.underlying_symbol == row.underlying_symbol
                && pk.settlement_symbol == row.settlement_symbol
            {
                db::confirm_from_indexer(
                    pool,
                    &pk.underlying_symbol,
                    &pk.settlement_symbol,
                    expiry,
                    &b.bucket_id.to_hex(),
                )?;
            }
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
