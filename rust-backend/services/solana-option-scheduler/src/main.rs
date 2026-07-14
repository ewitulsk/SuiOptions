//! solana-option-scheduler — bucket-creation lifecycle bot (Solana).
//!
//! Boot:
//!   1. Parse Cli + fetch the solana-token-info snapshot + load secrets.
//!   2. Connect the Solana RPC client; assert signer == program_info.admin
//!      (the only key `options_core` accepts for create_bucket — the
//!      parallel of the Sui twin's signer==deployer check).
//!   3. For each configured pair: resolve underlying/settlement catalog
//!      entries (mint, decimals, Pyth feed) and build a PairKey.
//!   4. Open the scheduler DB (MANDATORY) and run migrations. If the DB is
//!      unreachable the binary fails fast and exits — no in-memory fallback.
//!   5. Spawn the reconciler task. It is used ONLY to confirm submitted
//!      rolls and clear ambiguous ones — never to make a roll decision.
//!
//! Tick loop (every `tick_secs`, default 60):
//!   For each pair, read the latest active expiry FROM THE DB. If
//!   `latest_expiry - now < roll_threshold_ms`, compute the next expiry via
//!   the cadence, claim the slot in the DB (the partial UNIQUE index is the
//!   hard dedup), resolve the strike grid via current spot, record the
//!   derived bucket PDAs, and submit one `create_bucket` tx per strike
//!   (sequential, per-tx confirm; deterministic salts make re-runs collide
//!   Benign instead of duplicating).
//!
//! The DB is the single source of truth for which (pair, expiry) slots have
//! been rolled. The scheduler never relies on indexer-flowed state to decide
//! whether to roll.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use solana_indexer_graphql::{EventFilter, IndexerClient};
use solana_token_info_client::TokenInfoClient;
use solana_tx::SolanaClientWrapper;

use solana_option_scheduler::config::{GridConfig, PairConfig, SchedulerConfig, VaultTemplate};
use solana_option_scheduler::db;
use solana_option_scheduler::families::PairKey;
use solana_option_scheduler::reconcile::{self, ReconcileAction, SigStatus};
use solana_option_scheduler::roller::{self, ErrorClass, ProductType, RollPlan};
use solana_option_scheduler::schedule::{decide_tick, SkipReason, TickDecision};
use solana_option_scheduler::spot::ResolvedSpotSource;
use solana_option_scheduler::strike_grid::{build_strike_grid_for_pair, build_z_ladder_for_pair};
use solana_option_scheduler::vault_roller::{self, VaultPairSpec};
use solana_option_scheduler::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-option-scheduler");

    let cli = Cli::parse();
    let cfg = SchedulerConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    observability::ops::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    // Fetch the program ids + supported-token catalog from solana-token-info.
    // Hard cutover: if it is unreachable after the retry window we crash (no
    // solana-deployments.json fallback).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!(
                "fetching catalog from solana-token-info at {}",
                cli.token_info_url
            )
        })?;
    if snapshot.network() != cli.network.as_str() {
        warn!(
            cli_network = %cli.network,
            deployed_network = snapshot.network(),
            "--network differs from solana-token-info's deployment network"
        );
    }

    let wrap = Arc::new(SolanaClientWrapper::connect(&secrets, cli.network)?);

    // create_bucket / create_vault are admin-gated on-chain; a scheduler
    // signed by anyone else is useless and we'd rather fail loudly at boot
    // than hit a program revert on every tick (the Sui twin's
    // signer==deployer assertion).
    let admin = snapshot.admin();
    if wrap.signer.pubkey().to_string() != admin {
        return Err(anyhow!(
            "configured signer {} ≠ program admin {} from solana-token-info — \
             only the admin can create buckets/vaults",
            wrap.signer.pubkey(),
            admin
        ));
    }

    if cfg.pairs.is_empty() {
        return Err(anyhow!(
            "no [[pairs]] configured in {} — the scheduler would have nothing to do",
            cli.config.display()
        ));
    }

    // Spot + realized vol come from solana-oracle-service (the single Pyth
    // gateway) instead of the scheduler hitting Hermes/Benchmarks itself.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);

    // Resolve every configured pair against the /tokens catalog (mint,
    // decimals, Pyth feed). Pyth pairs without a feed id fail here, not at
    // first tick.
    let mut pair_keys: Vec<PairKey> = Vec::with_capacity(cfg.pairs.len());
    let mut pair_meta: Vec<PairMeta> = Vec::with_capacity(cfg.pairs.len());
    // Vault-eligible pairs: calls with a Pyth feed on BOTH legs (a vault
    // pins feed ids for its oracle reads, so feedless test-token pairs get
    // buckets but no vault). Empty unless a template is configured.
    let mut vault_entries: Vec<VaultEntry> = Vec::new();
    for pair in &cfg.pairs {
        let u_spec = snapshot.token_spec(&pair.underlying).with_context(|| {
            format!("underlying {} not in solana-token-info catalog", pair.underlying)
        })?;
        let s_spec = snapshot.token_spec(&pair.settlement).with_context(|| {
            format!("settlement {} not in solana-token-info catalog", pair.settlement)
        })?;
        let spot = ResolvedSpotSource::from_config(&pair.spot, u_spec, s_spec).with_context(
            || format!("resolving spot source for {}/{}", pair.underlying, pair.settlement),
        )?;
        let underlying_mint = Pubkey::from_str(&u_spec.mint)
            .with_context(|| format!("parsing {} mint {}", pair.underlying, u_spec.mint))?;
        let settlement_mint = Pubkey::from_str(&s_spec.mint)
            .with_context(|| format!("parsing {} mint {}", pair.settlement, s_spec.mint))?;
        let pair_key = PairKey {
            underlying_symbol: pair.underlying.clone(),
            settlement_symbol: pair.settlement.clone(),
            underlying_mint: u_spec.mint.clone(),
            settlement_mint: s_spec.mint.clone(),
        };
        // Effective vault template: per-pair override wins, else the global
        // template. Absent on both ⇒ no vault for this pair. Vault
        // auto-provisioning creates covered-CALL vaults only.
        if let Some(template) = pair
            .vault_template
            .clone()
            .or_else(|| cfg.vault_template.clone())
            .filter(|_| pair.product_type == ProductType::Call)
        {
            match (u_spec.pyth_feed(), s_spec.pyth_feed()) {
                (Ok(u_feed), Ok(s_feed)) => vault_entries.push(VaultEntry {
                    key: pair_key.clone(),
                    spec: VaultPairSpec {
                        underlying_symbol: pair.underlying.clone(),
                        settlement_symbol: pair.settlement.clone(),
                        underlying_mint: u_spec.mint.clone(),
                        settlement_mint: s_spec.mint.clone(),
                        underlying_decimals: u_spec.decimals,
                        settlement_decimals: s_spec.decimals,
                        underlying_feed_id: u_feed.0,
                        settlement_feed_id: s_feed.0,
                    },
                    template,
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
            underlying_mint,
            settlement_mint,
            underlying_decimals: u_spec.decimals,
            settlement_decimals: s_spec.decimals,
            spot,
        });
        info!(
            underlying = %pair.underlying,
            settlement = %pair.settlement,
            product = pair.product_type.as_str(),
            expiry_interval_ms = pair.expiry_interval_ms,
            spot = ?pair.spot,
            "pair configured"
        );
    }
    let pair_keys = Arc::new(pair_keys);

    // JIT indexer client. Used ONLY to confirm submitted rolls landed
    // (finalized tier) and to read the high-water sequence the reconciler
    // gates on — never to make a roll decision.
    let indexer = Arc::new(IndexerClient::new(cfg.indexer_graphql_url.clone()));

    // ── Scheduler DB (mandatory) ────────────────────────────────────
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
                    signature = ?row.signature,
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
    {
        let pool = db_pool.clone();
        let pair_keys = pair_keys.clone();
        let indexer = indexer.clone();
        let wrap = wrap.clone();
        let safety_margin = cfg.reconciler_safety_margin;
        let interval = Duration::from_secs(cfg.reconciler_interval_secs.max(1));
        tokio::spawn(async move {
            loop {
                metrics::counter!("scheduler_runs_total", "job" => "reconcile").increment(1);
                let started = Instant::now();
                if let Err(e) =
                    run_reconciler(&pool, &indexer, &wrap, &pair_keys, safety_margin).await
                {
                    warn!(error = %format!("{e:#}"), "reconciler tick errored");
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
        let started = Instant::now();
        if let Err(e) = tick_once(&cli, &cfg, &indexer, &pair_meta, &oracle, &wrap, &db_pool).await
        {
            warn!(error = %format!("{e:#}"), "tick errored");
        }
        metrics::histogram!("scheduler_job_duration_seconds", "job" => "roll")
            .record(started.elapsed().as_secs_f64());

        // Vault-ensure pass, gated to `vault_check_interval_ms`. Independent
        // of the bucket-roll cadence above.
        if !vault_entries.is_empty()
            && last_vault_check.is_none_or(|t| t.elapsed() >= vault_interval)
        {
            last_vault_check = Some(Instant::now());
            metrics::counter!("scheduler_runs_total", "job" => "vault").increment(1);
            let started = Instant::now();
            if let Err(e) = vault_pass(&cli, &indexer, &vault_entries, &wrap, &db_pool).await {
                warn!(error = %format!("{e:#}"), "vault pass errored");
            }
            metrics::histogram!("scheduler_job_duration_seconds", "job" => "vault")
                .record(started.elapsed().as_secs_f64());
        }

        sleep(tick).await;
    }
}

struct PairMeta {
    cfg: PairConfig,
    underlying_mint: Pubkey,
    settlement_mint: Pubkey,
    /// Cached at boot so the tick loop can compute chain-unit strikes
    /// against any `strike_scale` the planner picks.
    underlying_decimals: u8,
    settlement_decimals: u8,
    spot: ResolvedSpotSource,
}

/// A vault-eligible pair: the mint key (to match the indexer's `vaults`
/// view) plus everything `ensure_vault` needs to create one.
struct VaultEntry {
    key: PairKey,
    spec: VaultPairSpec,
    /// Effective vault policy for this pair (per-pair override or global).
    /// Its `round_ms` is also the cadence key that lets a weekly and an
    /// hourly vault for the same pair coexist.
    template: VaultTemplate,
}

/// Resolve the per-roll strike set: the legacy percentage grid, or the
/// vol-aware z-ladder when `[pairs.grid]` is configured. Both honor the
/// strike-scale integrality rule (see `strike_grid`).
async fn resolve_strikes(
    meta: &PairMeta,
    oracle: &oracle_client::OracleClient,
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

    let now_ms = now_ms();
    // σ: realized vol from the oracle service for live pairs; the configured
    // fallback covers static (test-token) pairs and outages.
    let sigma = match &meta.spot {
        ResolvedSpotSource::Pyth { underlying_feed, .. } => {
            match oracle.realized_vol(*underlying_feed, *vol_window_days).await {
                Ok(s) => s,
                Err(e) => {
                    let fallback = sigma_fallback.ok_or_else(|| {
                        anyhow!("realized vol fetch failed and no sigma_fallback: {e:#}")
                    })?;
                    warn!(error = %format!("{e:#}"), fallback, "realized vol fetch failed; using sigma_fallback");
                    fallback
                }
            }
        }
        ResolvedSpotSource::Static { .. } => sigma_fallback.ok_or_else(|| {
            anyhow!("z-ladder grid on a static-spot pair requires sigma_fallback")
        })?,
    };
    let sigma = sigma.clamp(*vol_floor, *vol_ceiling);

    let tau_ms = next_expiry_ms.saturating_sub(now_ms);
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
    oracle: &oracle_client::OracleClient,
    wrap: &SolanaClientWrapper,
    db_pool: &db::DbPool,
) -> Result<()> {
    let now = now_ms();
    for meta in pairs {
        let pair_label = format!("{}/{}", meta.cfg.underlying, meta.cfg.settlement);

        // The scheduler DB is the SOLE authority for what has been rolled.
        // A transient DB read error skips the pair this tick (retried next
        // tick) rather than risking a roll on missing state.
        let product_type = meta.cfg.product_type.as_str();
        let latest_expiry = match db::latest_active_expiry(
            db_pool,
            &meta.cfg.underlying,
            &meta.cfg.settlement,
            meta.cfg.expiry_interval_ms,
            product_type,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "latest_active_expiry failed; skipping pair this tick");
                continue;
            }
        };

        // Per-pair roll threshold wins over the global default.
        let roll_threshold_ms = meta.cfg.roll_threshold_ms.unwrap_or(cfg.roll_threshold_ms);
        let decision = decide_tick(
            latest_expiry,
            now,
            roll_threshold_ms,
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

        // The anchor is read JIT from the indexer head. If that read fails we
        // skip the roll this tick rather than stamp a bogus anchor — an
        // anchor that's too low could let the reconciler prematurely conclude
        // a real submit never landed.
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
        // Claim the slot. The partial UNIQUE index on (underlying,
        // settlement, expiry, product) is the hard guarantee.
        match db::claim_slot(
            db_pool,
            &meta.cfg.underlying,
            &meta.cfg.settlement,
            next_expiry,
            meta.cfg.expiry_interval_ms,
            product_type,
            anchor_seq,
        ) {
            Ok(true) => {
                debug!(pair = %pair_label, expiry_ms = next_expiry, "slot claimed");
            }
            Ok(false) => {
                debug!(pair = %pair_label, expiry_ms = next_expiry, "slot already claimed; skipping");
                continue;
            }
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "claim_slot failed; skipping");
                continue;
            }
        }

        let spot_usd_cross = match meta.spot.resolve_usd_cross(oracle).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %format!("{e:#}"), pair = %pair_label, "spot resolve failed; skipping");
                // Clean up the pending claim so the next tick can retry.
                let _ = db::delete_pending(
                    db_pool,
                    &meta.cfg.underlying,
                    &meta.cfg.settlement,
                    next_expiry,
                    product_type,
                );
                continue;
            }
        };
        let (strikes, strike_scale) =
            match resolve_strikes(meta, oracle, spot_usd_cross, next_expiry).await {
                Ok(g) => g,
                Err(e) => {
                    warn!(error = %format!("{e:#}"), pair = %pair_label, "strike grid invalid; skipping");
                    let _ = db::delete_pending(
                        db_pool,
                        &meta.cfg.underlying,
                        &meta.cfg.settlement,
                        next_expiry,
                        product_type,
                    );
                    continue;
                }
            };

        let plan = RollPlan {
            underlying_symbol: meta.cfg.underlying.clone(),
            settlement_symbol: meta.cfg.settlement.clone(),
            underlying_mint: meta.underlying_mint,
            settlement_mint: meta.settlement_mint,
            expiry_ms: next_expiry,
            strikes,
            strike_scale,
            product_type: meta.cfg.product_type,
        };
        plan.log_intent(cli.dry_run);

        if cli.dry_run {
            // Under dry-run, clean up the pending row since we won't submit.
            let _ = db::delete_pending(
                db_pool,
                &meta.cfg.underlying,
                &meta.cfg.settlement,
                next_expiry,
                product_type,
            );
            continue;
        }

        // Record the derived bucket PDAs UP FRONT so the reconciler can
        // resolve this roll against the indexer even if the submit loop
        // dies mid-family.
        let planned_ids: Vec<String> =
            plan.bucket_pdas().iter().map(|p| p.to_string()).collect();
        if let Err(e) = db::record_planned_buckets(
            db_pool,
            &meta.cfg.underlying,
            &meta.cfg.settlement,
            next_expiry,
            product_type,
            &planned_ids,
        ) {
            warn!(error = %e, pair = %pair_label, "record_planned_buckets failed");
        }

        // ── submit + classify ──────────────────────────────────────
        match roller::submit(wrap, &plan).await {
            Ok(out) => {
                metrics::counter!("scheduler_tx_total", "job" => "roll", "outcome" => "ok")
                    .increment(1);
                info!(
                    signature = ?out.signature,
                    bucket_count = out.bucket_ids.len(),
                    "rolled buckets submitted"
                );
                for id in &out.bucket_ids {
                    info!(bucket_id = %id, "new bucket");
                }
                if let Err(e) = db::mark_submitted(
                    db_pool,
                    &meta.cfg.underlying,
                    &meta.cfg.settlement,
                    next_expiry,
                    product_type,
                    out.signature.as_deref(),
                ) {
                    warn!(error = %e, "mark_submitted failed");
                }
            }
            Err(f) => {
                metrics::counter!("scheduler_tx_total", "job" => "roll", "outcome" => "error")
                    .increment(1);
                error!(
                    alert_id = "tx-failed-solana-option-scheduler",
                    error = %format!("{:#}", f.error),
                    class = ?f.class,
                    signature = ?f.signature,
                    pair = %pair_label,
                    "create_bucket submit failed"
                );
                match f.class {
                    ErrorClass::DefinitelyNotSent => {
                        // Retry next tick; already-created buckets collide
                        // Benign on the resume.
                        let _ = db::delete_pending(
                            db_pool,
                            &meta.cfg.underlying,
                            &meta.cfg.settlement,
                            next_expiry,
                            product_type,
                        );
                    }
                    ErrorClass::Ambiguous => {
                        let _ = db::mark_needs_reconciliation(
                            db_pool,
                            &meta.cfg.underlying,
                            &meta.cfg.settlement,
                            next_expiry,
                            product_type,
                            f.signature.as_deref(),
                            &format!("{:#}", f.error),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Vault-ensure pass: query the indexer's vault set once, then for each
/// vault-eligible pair create a vault if one doesn't already exist. Per-pair
/// errors are logged (with the vault alert id) and don't abort the pass.
async fn vault_pass(
    cli: &Cli,
    indexer: &IndexerClient,
    entries: &[VaultEntry],
    wrap: &SolanaClientWrapper,
    db_pool: &db::DbPool,
) -> Result<()> {
    let vaults = indexer.vaults().await.context("listing vaults for vault pass")?;
    for entry in entries {
        // Hard cutover: retire the DB row of any paused vault for this
        // pair+cadence so its active slot frees and a fresh replacement
        // rolls below. The retire bumps the pair's replacement generation
        // (retired-row count feeds the vault salt), so the replacement
        // derives a NEW PDA instead of colliding with the paused one.
        // Matched by vault id, so a live replacement is safe.
        for v in vaults.iter().filter(|v| {
            v.deposits_paused
                && entry.key.matches_mints(&v.underlying_mint, &v.settlement_mint)
                && v.round_ms == Some(entry.template.round_ms)
        }) {
            match db::retire_paused_vault(
                db_pool,
                &entry.spec.underlying_symbol,
                &entry.spec.settlement_symbol,
                entry.template.round_ms,
                &v.vault_id,
            ) {
                Ok(n) if n > 0 => info!(
                    pair = %format!("{}/{}", entry.spec.underlying_symbol, entry.spec.settlement_symbol),
                    vault_id = %v.vault_id,
                    "retired paused vault"
                ),
                Ok(_) => {}
                Err(e) => warn!(error = %format!("{e:#}"), "retire_paused_vault failed"),
            }
        }
        // Match the on-chain vault by mints AND round cadence: a weekly and
        // an hourly vault for the same pair are distinct. A paused vault is
        // decommissioned — never adopt it as this pair's live vault.
        let existing = vaults
            .iter()
            .find(|v| {
                entry.key.matches_mints(&v.underlying_mint, &v.settlement_mint)
                    && v.round_ms == Some(entry.template.round_ms)
                    && !v.deposits_paused
            })
            .map(|v| v.vault_id.clone());
        if let Err(e) = vault_roller::ensure_vault(
            wrap,
            db_pool,
            &entry.spec,
            &entry.template,
            existing,
            cli.dry_run,
        )
        .await
        {
            error!(
                alert_id = "tx-failed-solana-option-scheduler-vault",
                pair = %format!("{}/{}", entry.spec.underlying_symbol, entry.spec.settlement_symbol),
                error = %format!("{e:#}"),
                "ensure_vault failed"
            );
        }
    }
    Ok(())
}

/// Confirmation + reconciliation pass (JIT).
///
/// Order per the service guide: (1) confirm rows whose planned buckets all
/// appear in **finalized** BucketCreated events; (2) supersede confirmed
/// families the chain fully invalidated; (3) resolve leftover rows —
/// `getSignatureStatuses` FIRST (definitive on Solana), then the
/// indexer-anchor rule for whatever the status check couldn't decide.
async fn run_reconciler(
    pool: &db::DbPool,
    indexer: &IndexerClient,
    wrap: &SolanaClientWrapper,
    pair_keys: &[PairKey],
    safety_margin: u64,
) -> Result<()> {
    confirm_landed_rolls(pool, indexer, pair_keys).await?;
    supersede_invalidated_families(pool, indexer, pair_keys).await?;
    resolve_unconfirmed_rolls(pool, indexer, wrap, safety_margin).await?;
    Ok(())
}

/// The mint pair for a roll row's configured symbols, if the pair is still
/// configured on this instance.
fn mints_for_row<'a>(
    pair_keys: &'a [PairKey],
    underlying_symbol: &str,
    settlement_symbol: &str,
) -> Option<&'a PairKey> {
    pair_keys.iter().find(|p| {
        p.underlying_symbol == underlying_symbol && p.settlement_symbol == settlement_symbol
    })
}

/// The finalized-tier BucketCreated/PutBucketCreated bucket ids for a roll
/// row's (pair, expiry), scanned from the row's submit anchor.
async fn landed_bucket_ids(
    indexer: &IndexerClient,
    key: &PairKey,
    row: &db::models::SchedulerRollRow,
) -> Result<HashSet<String>> {
    let event_type = if row.product_type == ProductType::Put.as_str() {
        "PutBucketCreated"
    } else {
        "BucketCreated"
    };
    // Payload numeric values are decimal strings in the raw event JSON.
    let filter = EventFilter::new()
        .event_types([event_type])
        .payload_contains(json!({
            "underlying_mint": key.underlying_mint,
            "settlement_mint": key.settlement_mint,
            "expiry_ms": row.expiry_ms.to_string(),
        }));
    let after = row.submit_anchor_seq.unwrap_or(0).max(0) as u64;
    // finalized_only: roll confirmation is fold-into-own-state — only the
    // reorg-proof tier counts.
    let events = indexer.scan_events(&filter, after, true).await?;
    let mut out = HashSet::new();
    for ev in &events {
        if let Ok(bucket) = ev.payload_str("bucket") {
            out.insert(bucket.to_string());
        }
    }
    Ok(out)
}

/// For each submitted / needs_reconciliation row, JIT-query the indexer for
/// finalized BucketCreated events matching the row's pair+expiry, and confirm
/// the row once EVERY planned bucket PDA is visible.
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
        let planned = row.planned_bucket_ids();
        if planned.is_empty() {
            continue; // nothing recorded to confirm against
        }
        let Some(key) = mints_for_row(pair_keys, &row.underlying_symbol, &row.settlement_symbol)
        else {
            continue;
        };
        let landed = landed_bucket_ids(indexer, key, &row).await?;
        if planned.iter().all(|id| landed.contains(id)) {
            info!(
                id = row.id,
                pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
                expiry_ms = row.expiry_ms,
                buckets = planned.len(),
                "reconciler: all planned buckets finalized — confirming roll"
            );
            db::confirm_row(pool, row.id, &planned)?;
        }
    }
    Ok(())
}

/// Treat a fully-invalidated bucket family as if it never existed: mark its
/// confirmed roll `superseded` so the active slot frees and the cadence
/// picker re-rolls a fresh family at the same expiry. Only confirmed,
/// unexpired rolls are checked. Non-empty guard: an empty result means the
/// indexer hasn't ingested the family yet (or it's been cleaned) — don't
/// supersede on that.
async fn supersede_invalidated_families(
    pool: &db::DbPool,
    indexer: &IndexerClient,
    pair_keys: &[PairKey],
) -> Result<()> {
    let rows = db::confirmed_unexpired_rolls(pool, now_ms())?;
    for row in rows {
        let Some(key) = mints_for_row(pair_keys, &row.underlying_symbol, &row.settlement_symbol)
        else {
            continue;
        };
        // Live (non-cleaned) buckets at this expiry belonging to this pair
        // and product.
        let family = indexer
            .buckets(
                true,
                None,
                Some(&key.underlying_mint),
                Some(&key.settlement_mint),
                Some(row.expiry_ms as u64),
                Some(&row.product_type),
            )
            .await?;
        if !family.is_empty() && family.iter().all(|b| b.invalidated) {
            info!(
                id = row.id,
                pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
                expiry_ms = row.expiry_ms,
                buckets = family.len(),
                "reconciler: bucket family fully invalidated — superseding so the cadence picker re-rolls"
            );
            metrics::counter!("scheduler_rolls_superseded_total").increment(1);
            db::mark_superseded(pool, row.id)?;
        }
    }
    Ok(())
}

/// Resolve rows the confirm pass left behind. `getSignatureStatuses` on the
/// recorded signature is checked FIRST (definitive on Solana); the
/// indexer-anchor rule covers everything the status can't decide. A deleted
/// row frees the slot for a re-claim — the resume is salt-idempotent.
async fn resolve_unconfirmed_rolls(
    pool: &db::DbPool,
    indexer: &IndexerClient,
    wrap: &SolanaClientWrapper,
    safety_margin: u64,
) -> Result<()> {
    // Defensive sweep over submitted rows: a signature that landed-with-err
    // (reorg edge) means the family will never confirm — demote it so the
    // needs_reconciliation resolution below picks it up.
    for row in db::all_active_rows(pool)? {
        if row.state != "submitted" {
            continue;
        }
        if let SigStatus::Failed = sig_status(wrap, row.signature.as_deref()).await {
            warn!(
                id = row.id,
                signature = ?row.signature,
                "reconciler: submitted roll's signature failed on-chain — demoting to needs_reconciliation"
            );
            db::demote_submitted_to_reconciliation(
                pool,
                row.id,
                "signature landed with an on-chain error",
            )?;
        }
    }

    let rows = db::needs_reconciliation_rows(pool)?;
    if rows.is_empty() {
        return Ok(());
    }
    let head_seq = indexer.head_sequence().await?;
    for row in rows {
        let sig = sig_status(wrap, row.signature.as_deref()).await;
        let anchor = row.submit_anchor_seq.unwrap_or(0).max(0) as u64;
        // The confirm pass already flipped fully-landed rows, so a row here
        // is by definition not fully confirmed.
        match reconcile::decide(false, sig, head_seq, anchor, safety_margin) {
            ReconcileAction::Confirm => unreachable!("confirm pass handles landed families"),
            ReconcileAction::Delete => {
                info!(
                    id = row.id,
                    pair = %format!("{}/{}", row.underlying_symbol, row.settlement_symbol),
                    expiry_ms = row.expiry_ms,
                    sig = ?sig,
                    head_seq,
                    anchor,
                    "reconciler: roll never fully landed — clearing for salt-idempotent re-claim"
                );
                db::delete_reconciled(pool, row.id)?;
            }
            ReconcileAction::Wait => {
                debug!(id = row.id, sig = ?sig, head_seq, anchor, "reconciler: waiting");
            }
        }
    }
    Ok(())
}

/// Map `getSignatureStatuses` for one recorded signature to the pure
/// decision enum. Missing/unparseable signatures and RPC errors read as
/// NotFound — the anchor rule then decides, never a lone status hiccup.
async fn sig_status(wrap: &SolanaClientWrapper, signature: Option<&str>) -> SigStatus {
    let Some(raw) = signature else {
        return SigStatus::NotFound;
    };
    let Ok(sig) = Signature::from_str(raw) else {
        warn!(signature = raw, "unparseable signature on roll row");
        return SigStatus::NotFound;
    };
    match wrap.client.get_signature_statuses(&[sig]).await {
        Ok(resp) => match resp.value.into_iter().next().flatten() {
            Some(status) if status.err.is_some() => SigStatus::Failed,
            Some(_) => SigStatus::Landed,
            None => SigStatus::NotFound,
        },
        Err(e) => {
            warn!(error = %e, signature = raw, "getSignatureStatuses failed; treating as not found");
            SigStatus::NotFound
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
