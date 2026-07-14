//! solana-keeper — permissionless crank-driver for the covered-call
//! vaults (guide doc 09; the Solana port of `services/keeper`).
//!
//! Boot:
//!   1. Parse Cli, load config + secrets, fetch the solana-token-info
//!      snapshot (hard cutover) and cross-check its program ids against
//!      the compiled-in program crates (drift ⇒ crash: the builders would
//!      target the wrong deployment).
//!   2. Connect the RPC wrapper. Any funded wallet works — the keeper
//!      holds no privileged accounts; `options_vault` validates every
//!      crank.
//!
//! Tick loop (every `tick_secs`, default 15):
//!   1. **Discover** vaults from the indexer's `vaults` view and resolve
//!      each new one's mints / pinned feeds / decimals from its chain
//!      account ([`solana_keeper::discovery`]).
//!   2. Per vault: read the chain ([`state::fetch_vault_view`] + open
//!      auction discovery), plan the single next action
//!      ([`planner::plan`]), resolve a strike pick when asked, and submit
//!      — with fresh Pyth `PriceUpdateV2` posts ahead of oracle-gated
//!      cranks ([`submit`] / [`pyth_leg`]).
//!
//! Fatal errors (config bugs) halt that vault until restart; everything
//! else replans from fresh state next tick.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use solana_sdk::pubkey::Pubkey;
use tokio::time::sleep;
use tracing::{debug, error, info, trace, warn};

use solana_indexer_graphql::IndexerClient;
use solana_token_info_client::TokenInfoClient;
use solana_tx::SolanaClientWrapper;

use solana_keeper::config::{KeeperConfig, VaultDefaults};
use solana_keeper::discovery::{resolve_vault, DiscoveredVault};
use solana_keeper::planner::{plan, Action, BucketMeta, PlanInput};
use solana_keeper::pyth_leg::PythPoster;
use solana_keeper::state::{discover_open_auctions, fetch_vault_view, VaultView};
use solana_keeper::strike::{pick_bucket, BucketCandidate};
use solana_keeper::submit::{classify, execute, execute_select_bucket, ErrorClass, SubmitCtx};
use solana_keeper::{slicing, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("solana-keeper");

    let cli = Cli::parse();
    let cfg = KeeperConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    observability::ops::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!("fetching catalog from solana-token-info at {}", cli.token_info_url)
        })?;
    // Program-id drift between the deployed registry and the compiled-in
    // program crates would make every builder target the wrong programs.
    for (name, registry, compiled) in [
        ("options_core", snapshot.core_program(), options_core::ID.to_string()),
        ("auction_venue", snapshot.venue_program(), auction_venue::ID.to_string()),
        ("options_vault", snapshot.vault_program(), options_vault::ID.to_string()),
    ] {
        if registry != compiled {
            return Err(anyhow!(
                "{name} program id mismatch: token-info says {registry}, this binary was \
                 compiled against {compiled} — redeploy the keeper against the live programs"
            ));
        }
    }

    let wrap = SolanaClientWrapper::connect(&secrets, cli.network)?;
    info!(signer = %wrap.signer.pubkey(), "keeper wallet connected (gas only)");

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .default_headers(pyth_client::auth_headers(secrets.pyth_api_key()))
        .build()
        .context("building reqwest client")?;
    let wormhole_program = match &cfg.pyth.wormhole_program_id {
        Some(s) => Pubkey::from_str(s)
            .with_context(|| format!("parsing pyth.wormhole_program_id {s:?}"))?,
        None => solana_tx::pyth::WORMHOLE_RECEIVER_ID,
    };
    let mut poster =
        PythPoster::new(cfg.pyth.hermes_url.clone(), wormhole_program, wrap.signer.pubkey());

    // Spot + realized vol come from solana-oracle-service (the single Pyth
    // read gateway); the keeper's own `http` stays for the on-chain update
    // data, which a price cache can't serve. Hard cutover: crash if the
    // oracle never comes up.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    oracle
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("solana-oracle-service at {} unreachable", cli.oracle_url))?;
    let indexer = IndexerClient::new(cfg.indexer_graphql_url.clone());
    info!(
        indexer = %cfg.indexer_graphql_url,
        target_delta = cfg.vault_defaults.target_delta,
        iv_ratio = cfg.vault_defaults.iv_ratio,
        "vault auto-discovery enabled"
    );

    // Discovered vaults (resolution is immutable per vault) and vaults
    // halted on a Fatal classification; both cleared only by restart.
    let mut vaults: HashMap<Pubkey, DiscoveredVault> = HashMap::new();
    let mut halted: HashSet<Pubkey> = HashSet::new();
    // Vaults with deposits paused on-chain: a hard cutover signal — the
    // keeper ignores them entirely (no cranks of any kind). Rebuilt from
    // the indexer every tick, so pause/unpause takes effect live.
    let mut paused: HashSet<Pubkey> = HashSet::new();

    let tick = Duration::from_secs(cfg.tick_secs.max(1));
    info!(tick_secs = cfg.tick_secs, dry_run = cli.dry_run, "tick loop starting");
    loop {
        metrics::counter!("solana_keeper_ticks_total").increment(1);
        let tick_started = std::time::Instant::now();
        discover_new_vaults(&wrap, &indexer, &mut vaults, &halted, &mut paused).await;
        metrics::gauge!("solana_keeper_vaults_discovered").set(vaults.len() as f64);

        for (id, meta) in &vaults {
            if halted.contains(id) || paused.contains(id) {
                continue;
            }
            match tick_vault(&cli, &wrap, &http, &mut poster, &oracle, &indexer, meta, &cfg.vault_defaults)
                .await
            {
                Ok(()) => {}
                Err(e) => match classify(&e) {
                    ErrorClass::Benign => {
                        debug!(vault = %id, error = %format!("{e:#}"), "lost a race; replanning next tick");
                    }
                    ErrorClass::Retry => {
                        error!(
                            alert_id = "tx-failed-solana-keeper",
                            vault = %id,
                            class = "retry",
                            error = %format!("{e:#}"),
                            "transient tx failure; retrying next tick"
                        );
                    }
                    ErrorClass::Fatal => {
                        error!(
                            alert_id = "tx-failed-solana-keeper",
                            vault = %id,
                            class = "fatal",
                            error = %format!("{e:#}"),
                            "FATAL: halting this vault until restart"
                        );
                        halted.insert(*id);
                    }
                },
            }
        }
        metrics::histogram!("solana_keeper_tick_duration_seconds")
            .record(tick_started.elapsed().as_secs_f64());
        sleep(tick).await;
    }
}

/// One discovery pass: list vaults from the indexer and fully resolve
/// any we haven't seen. Failures are logged and retried next tick — a
/// half-resolved vault is never cranked.
async fn discover_new_vaults(
    wrap: &SolanaClientWrapper,
    indexer: &IndexerClient,
    vaults: &mut HashMap<Pubkey, DiscoveredVault>,
    halted: &HashSet<Pubkey>,
    paused: &mut HashSet<Pubkey>,
) {
    let rows = match indexer.vaults().await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %format!("{e:#}"), "vault discovery query failed; keeping known vaults");
            return;
        }
    };
    // Refresh the paused set from this tick's snapshot so an admin
    // pause/unpause is honored without a keeper restart.
    paused.clear();
    for row in &rows {
        let Ok(id) = Pubkey::from_str(&row.vault_id) else {
            warn!(vault = %row.vault_id, "unparseable vault id from indexer");
            continue;
        };
        if row.deposits_paused {
            paused.insert(id);
        }
        if vaults.contains_key(&id) || halted.contains(&id) {
            continue;
        }
        match resolve_vault(wrap, &id).await {
            Ok(v) => {
                info!(
                    vault = %id,
                    pair = %format!("{}/{}", v.underlying_mint, v.settlement_mint),
                    underlying_feed = %v.underlying_feed,
                    settlement_feed = %v.settlement_feed,
                    "vault discovered"
                );
                vaults.insert(id, v);
            }
            Err(e) => {
                warn!(vault = %id, error = %format!("{e:#}"), "vault resolution failed; retrying next tick");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn tick_vault(
    cli: &Cli,
    wrap: &SolanaClientWrapper,
    http: &reqwest::Client,
    poster: &mut PythPoster,
    oracle: &oracle_client::OracleClient,
    indexer: &IndexerClient,
    meta: &DiscoveredVault,
    defaults: &VaultDefaults,
) -> Result<()> {
    let now = now_ms();
    let view = fetch_vault_view(wrap, &meta.vault).await?;

    let (auctions, swap_auctions) = if view.open_rfqs > 0 || view.open_swap_rfqs > 0 {
        discover_open_auctions(indexer, wrap, &meta.vault).await?
    } else {
        (Vec::new(), Vec::new())
    };

    // The current bucket's invalidation flag from the indexer.
    let bucket_meta = match view.current_bucket {
        Some(bucket) => Some(fetch_bucket_meta(indexer, &bucket).await?),
        None => None,
    };

    // Cap both the slice count and the stagger to this vault's selling
    // window: an hourly round's 30-min window runs a single slice, while a
    // weekly round's 12h window keeps the configured count (self-scaling).
    let slices =
        slicing::effective_slices(defaults.slicing.slices, view.config.selling_window_ms);
    let stagger_ms = slicing::effective_stagger_ms(
        defaults.slicing.stagger_minutes * 60_000,
        view.config.selling_window_ms,
        slices,
    );

    let action = plan(&PlanInput {
        view: &view,
        now_ms: now,
        auctions: &auctions,
        swap_auctions: &swap_auctions,
        bucket_meta: bucket_meta.as_ref(),
        stagger_ms,
        max_slices: slices,
    });
    // Skip the steady-state Idle plan to avoid a per-vault log flood every
    // tick; only surface a plan when there's actually an action to take.
    if !matches!(action, Action::Idle) {
        debug!(vault = %meta.vault, round = view.round, ?action, "planned");
    }

    let mut ctx = SubmitCtx { wrap, http, poster, meta };
    match action {
        Action::Idle => Ok(()),
        Action::SelectBucketNeeded => {
            select_bucket_or_finalize(cli, oracle, indexer, &mut ctx, defaults, &view, now).await
        }
        other => {
            if cli.dry_run {
                info!(vault = %meta.vault, action = ?other, "dry-run: would submit");
                return Ok(());
            }
            execute(&mut ctx, &view, &other).await
        }
    }
}

/// Resolve `SelectBucketNeeded`: σ + spot + candidates → pick →
/// `select_bucket`. No viable candidate: finalize if queued flows are
/// waiting on the round to roll, otherwise idle.
async fn select_bucket_or_finalize(
    cli: &Cli,
    oracle: &oracle_client::OracleClient,
    indexer: &IndexerClient,
    ctx: &mut SubmitCtx<'_>,
    defaults: &VaultDefaults,
    view: &VaultView,
    now: u64,
) -> Result<()> {
    let meta = ctx.meta;
    let candidates = fetch_candidates(indexer, meta).await?;
    let spot = fetch_spot_cross(oracle, meta).await?;
    let sigma = fetch_sigma(oracle, meta, defaults).await?;
    let sigma_iv = sigma * defaults.iv_ratio;

    let pick = pick_bucket(
        &candidates,
        spot,
        sigma_iv,
        now,
        &view.config,
        meta.underlying_decimals,
        meta.settlement_decimals,
        defaults.target_delta_for(view.config.round_ms),
    );

    // Skip rounds whose snapped strike can't clear the reserve the
    // on-chain open_rfq will set: the option's fair value caps any
    // plausible bid, so a sub-reserve strike can only churn open/settle
    // fees on auctions that always expire unsold.
    match pick {
        Some(p) if p.clears_reserve(spot, view.config.min_reserve_premium_bps) => {
            // The calibration trail: every selection logs
            // (σ, K*, snapped strike, model delta).
            info!(
                vault = %meta.vault,
                round = view.round,
                spot,
                sigma,
                sigma_iv,
                k_star = p.k_star_usd,
                strike = p.strike_usd,
                model_delta = p.model_delta,
                expiry_ms = p.expiry_ms,
                grid_coverage_miss = p.grid_coverage_miss,
                bucket = %p.bucket,
                "strike pick"
            );
            if p.grid_coverage_miss {
                warn!(vault = %meta.vault, "GridCoverageMiss: no candidate ≥ K* — check the scheduler grid");
            }
            if cli.dry_run {
                info!(vault = %meta.vault, "dry-run: would select_bucket");
                return Ok(());
            }
            execute_select_bucket(ctx, &p.bucket).await
        }
        Some(p) => {
            // A strike exists but its fair value can't clear the reserve —
            // skip the round rather than churn unsellable auctions.
            info!(
                vault = %meta.vault,
                round = view.round,
                spot,
                sigma_iv,
                strike = p.strike_usd,
                model_delta = p.model_delta,
                model_premium_usd = p.model_premium_usd,
                reserve_per_unit = spot * view.config.min_reserve_premium_bps as f64 / 10_000.0,
                "strike unsellable: fair value below reserve — idling round"
            );
            idle_or_finalize_round(cli, ctx, view).await
        }
        None => idle_or_finalize_round(cli, ctx, view).await,
    }
}

/// No viable strike this round — no in-band candidate, or none whose fair
/// value clears the reserve. Finalize if deposits/withdrawals are queued
/// and waiting on the round to roll, otherwise idle.
async fn idle_or_finalize_round(
    cli: &Cli,
    ctx: &mut SubmitCtx<'_>,
    view: &VaultView,
) -> Result<()> {
    let flows_waiting = view.pending_deposits > 0 || view.queued_withdraw_shares > 0;
    if !flows_waiting {
        trace!(vault = %ctx.meta.vault, "no viable strike and no queued flows; idling");
        return Ok(());
    }
    warn!(
        vault = %ctx.meta.vault,
        round = view.round,
        "no viable strike but flows are queued — finalizing the idle round"
    );
    if cli.dry_run {
        info!(vault = %ctx.meta.vault, "dry-run: would finalize_round (idle)");
        return Ok(());
    }
    execute(ctx, view, &Action::FinalizeRound).await
}

async fn fetch_bucket_meta(indexer: &IndexerClient, bucket: &Pubkey) -> Result<BucketMeta> {
    let b = indexer
        .bucket(&bucket.to_string())
        .await
        .context("fetching the current round's bucket")?
        .ok_or_else(|| anyhow!("current bucket {bucket} not in indexer — lagging?"))?;
    Ok(BucketMeta { invalidated: b.invalidated })
}

async fn fetch_candidates(
    indexer: &IndexerClient,
    meta: &DiscoveredVault,
) -> Result<Vec<BucketCandidate>> {
    let buckets = indexer
        .buckets(
            true,
            None,
            Some(&meta.underlying_mint.to_string()),
            Some(&meta.settlement_mint.to_string()),
            None,
            Some("call"),
        )
        .await
        .context("fetching candidate buckets")?;
    buckets
        .into_iter()
        .filter(|b| !b.invalidated && !b.cleaned)
        .map(|b| {
            Ok(BucketCandidate {
                bucket: Pubkey::from_str(&b.bucket_id)
                    .with_context(|| format!("parsing bucket id {:?}", b.bucket_id))?,
                strike_raw: b.strike,
                strike_scale: b.strike_scale,
                expiry_ms: b.expiry_ms,
            })
        })
        .collect()
}

/// USD cross (settlement-per-underlying) from the oracle-service price cache.
async fn fetch_spot_cross(
    oracle: &oracle_client::OracleClient,
    meta: &DiscoveredVault,
) -> Result<f64> {
    let u = oracle
        .price(meta.underlying_feed)
        .await
        .context("fetching underlying spot from oracle-service")?
        .price;
    let s = oracle
        .price(meta.settlement_feed)
        .await
        .context("fetching settlement spot from oracle-service")?
        .price;
    if !(u > 0.0 && s > 0.0) {
        return Err(anyhow!("non-positive oracle prices: {u} / {s}"));
    }
    Ok(u / s)
}

/// Realized σ from oracle-service (cached/paced Benchmarks), with the
/// configured static fallback for outages.
async fn fetch_sigma(
    oracle: &oracle_client::OracleClient,
    meta: &DiscoveredVault,
    defaults: &VaultDefaults,
) -> Result<f64> {
    match oracle
        .realized_vol(meta.underlying_feed, defaults.vol_window_days)
        .await
    {
        Ok(s) => Ok(s),
        Err(e) => match defaults.sigma_fallback {
            Some(fallback) => {
                warn!(error = %format!("{e:#}"), fallback, "realized vol fetch failed; using sigma_fallback");
                Ok(fallback)
            }
            None => Err(e.context("realized vol fetch failed and no sigma_fallback")),
        },
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
