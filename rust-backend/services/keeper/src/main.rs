//! vault-keeper — permissionless crank-driver for the covered-call
//! vaults (`services/keeper/README.md` is the build spec).
//!
//! Boot:
//!   1. Parse Cli, load config + secrets, fetch the token-info snapshot
//!      (protocol ids, DEEP coin type).
//!   2. Connect SuiClient. Any funded wallet works — the keeper holds no
//!      capability objects; `vault.move` validates every crank.
//!
//! Tick loop (every `tick_secs`, default 15):
//!   1. **Discover** vaults from the indexer's `vaults` view (fed by the
//!      on-chain `VaultCreated` event) and resolve each new one's pinned
//!      feeds / decimals / `PriceInfoObject`s from chain state
//!      ([`keeper::discovery`]) — a vault created on chain is picked up
//!      on the next tick, no config change needed.
//!   2. Per vault: read the chain ([`state::fetch_vault_view`] +
//!      AuctionCreated discovery), plan the single next action
//!      ([`planner::plan`]), resolve a strike pick when asked, and
//!      submit with a Pyth price update prepended in-PTB ([`submit`]).
//!
//! Fatal errors (config bugs) halt that vault until restart; everything
//! else replans from fresh state next tick.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::{debug, error, info, trace, warn};

use indexer_graphql::IndexerClient;
use protocol_types::asset::AssetType;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::pyth_update::PythHandles;
use sui_types::base_types::ObjectID;
use token_info_client::TokenInfoClient;

use keeper::config::{KeeperConfig, VaultDefaults};
use keeper::discovery::{
    resolve_price_info_table, resolve_vault, DiscoveredVault, PriceInfoTable,
};
use keeper::planner::{plan, Action, BucketMeta, PlanInput};
use keeper::state::{discover_open_auctions, fetch_vault_view, VaultView};
use keeper::strike::{pick_bucket, BucketCandidate};
use keeper::submit::{classify, execute, execute_select_bucket, ErrorClass, SubmitCtx};
use keeper::Cli;

/// How far back to scan AuctionCreated events for live auctions: auctions
/// can't outlive a round, so two round lengths is generous.
const RFQ_LOOKBACK_ROUNDS: u64 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("keeper");

    let cli = Cli::parse();
    let cfg = KeeperConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    observability::ops::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    // Four-package split: cranks target options_vault; auction discovery
    // filters the generic auction package's events. Both are required —
    // fail at boot on a deployment without them.
    let vault_package = snapshot
        .vault()
        .context("options_vault package missing from token-info package_info")?
        .package()?;
    let auction_package = snapshot
        .auction()
        .context("auction package missing from token-info package_info")?
        .package()?;
    let protocol_config_id = snapshot.protocol_config()?;
    let treasury_id = snapshot.treasury()?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    info!(signer = %wrap.signer.address, "keeper wallet connected (gas only)");


    let pyth_handles = PythHandles {
        pyth_package: parse_id(&cfg.pyth.pyth_package_id, "pyth_package_id")?,
        wormhole_package: parse_id(&cfg.pyth.wormhole_package_id, "wormhole_package_id")?,
        pyth_state_id: parse_id(&cfg.pyth.pyth_state_id, "pyth_state_id")?,
        wormhole_state_id: parse_id(&cfg.pyth.wormhole_state_id, "wormhole_state_id")?,
        update_fee_mist: cfg.pyth.update_fee_mist,
    };

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .default_headers(pyth_client::auth_headers(secrets.pyth_api_key()))
        .build()
        .context("building reqwest client")?;
    // Spot + realized vol now come from oracle-service (the single Pyth
    // gateway) instead of the keeper hitting Hermes/Benchmarks itself. The
    // keeper's own `http` stays for the on-chain VAA path (submit.rs), which a
    // price cache can't serve. Hard cutover: crash if the oracle never comes up.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    oracle
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("oracle-service at {} unreachable", cli.oracle_url))?;
    // Trading-vault pass (SO-287/290). Governance ids prefer
    // token-info's recorded block, falling back to publish-effects
    // discovery. A build failure is fatal (partial token-info snapshot
    // from a same-wave deploy boot race) — crash and let the supervisor
    // retry against a warmed token-info.
    let trading_vault_ctx = keeper::trading_vault::build_ctx(
        &wrap.client,
        &snapshot,
        treasury_id,
        protocol_config_id,
        cli.gas_budget,
        cfg.pyth.hermes_url.clone(),
        pyth_handles.clone(),
        &cfg.external,
        oracle.clone(),
        cfg.vault_defaults.vol_window_days,
    )
    .await
    .context("building the trading-vault ctx (token-info snapshot incomplete or chain unreachable)")?;
    let indexer = IndexerClient::new(cfg.indexer_graphql_url.clone());
    info!(
        indexer = %cfg.indexer_graphql_url,
        target_delta = cfg.vault_defaults.target_delta,
        iv_ratio = cfg.vault_defaults.iv_ratio,
        "vault auto-discovery enabled"
    );

    // Discovered vaults (resolution is immutable per vault) and vaults
    // halted on a Fatal classification; both cleared only by restart.
    let mut vaults: HashMap<ObjectID, DiscoveredVault> = HashMap::new();
    let mut halted: HashSet<ObjectID> = HashSet::new();
    // Vaults with deposits paused on-chain: a hard cutover signal — the
    // keeper ignores them entirely (no cranks of any kind). Rebuilt from
    // the indexer every tick, so pause/unpause takes effect live.
    let mut paused: HashSet<ObjectID> = HashSet::new();
    // The Pyth feed → PriceInfoObject table handle, resolved lazily on
    // the first discovery so an indexer-only outage doesn't block boot.
    let mut price_table: Option<PriceInfoTable> = None;

    let tick = Duration::from_secs(cfg.tick_secs.max(1));
    info!(tick_secs = cfg.tick_secs, dry_run = cli.dry_run, "tick loop starting");
    loop {
        metrics::counter!("keeper_ticks_total").increment(1);
        let tick_started = std::time::Instant::now();
        discover_new_vaults(
            &wrap,
            &indexer,
            &pyth_handles,
            &mut price_table,
            &mut vaults,
            &halted,
            &mut paused,
        )
        .await;
        metrics::gauge!("keeper_vaults_discovered").set(vaults.len() as f64);

        for (id, meta) in &vaults {
            if halted.contains(id) || paused.contains(id) {
                continue;
            }
            let ctx = TickCtx {
                vault_package,
                auction_package,
                protocol_config_id,
                treasury_id,
                defaults: &cfg.vault_defaults,
            };
            match tick_vault(&cli, &cfg, &wrap, &http, &oracle, &indexer, &pyth_handles, meta, ctx).await {
                Ok(()) => {}
                Err(e) => match classify(&e) {
                    ErrorClass::Benign => {
                        debug!(vault = %id, error = %format!("{e:#}"), "lost a race; replanning next tick");
                    }
                    ErrorClass::Retry => {
                        error!(
                            alert_id = "tx-failed-keeper",
                            vault = %id,
                            class = "retry",
                            error = %format!("{e:#}"),
                            "transient tx failure; retrying next tick"
                        );
                    }
                    ErrorClass::Fatal => {
                        error!(
                            alert_id = "tx-failed-keeper",
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
        if let Some(tvc) = &trading_vault_ctx {
            keeper::trading_vault::tick(&wrap, &http, &indexer, tvc).await;
        }
        metrics::histogram!("keeper_tick_duration_seconds").record(tick_started.elapsed().as_secs_f64());
        sleep(tick).await;
    }
}

/// One discovery pass: list vaults from the indexer and fully resolve
/// any we haven't seen. Failures are logged and retried next tick — a
/// half-resolved vault is never cranked.
async fn discover_new_vaults(
    wrap: &SuiClientWrapper,
    indexer: &IndexerClient,
    pyth_handles: &PythHandles,
    price_table: &mut Option<PriceInfoTable>,
    vaults: &mut HashMap<ObjectID, DiscoveredVault>,
    halted: &HashSet<ObjectID>,
    paused: &mut HashSet<ObjectID>,
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
        if row.deposits_paused {
            paused.insert(ObjectID::new(*row.vault_id.as_bytes()));
        }
    }
    for row in rows {
        let id = ObjectID::new(*row.vault_id.as_bytes());
        if vaults.contains_key(&id) || halted.contains(&id) {
            continue;
        }
        // Lazily resolve the Pyth state's feed → PriceInfoObject table.
        if price_table.is_none() {
            match resolve_price_info_table(&wrap.client, pyth_handles.pyth_state_id).await {
                Ok(t) => *price_table = Some(t),
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "pyth price_info table resolution failed; retrying next tick");
                    return;
                }
            }
        }
        let table = price_table.as_ref().expect("resolved above");
        match resolve_vault(&wrap.client, &row, table).await {
            Ok(v) => {
                info!(
                    vault = %id,
                    pair = %format!("{}/{}", v.underlying_type, v.settlement_type),
                    underlying_feed = %v.underlying_feed,
                    underlying_price_info = %v.underlying_price_info,
                    settlement_price_info = %v.settlement_price_info,
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

/// Per-tick context shared by every vault (resolved once at boot).
#[derive(Clone, Copy)]
struct TickCtx<'a> {
    /// options_vault package (crank targets).
    vault_package: ObjectID,
    /// Generic auction package (AuctionCreated discovery).
    auction_package: ObjectID,
    protocol_config_id: ObjectID,
    treasury_id: ObjectID,
    defaults: &'a VaultDefaults,
}

#[allow(clippy::too_many_arguments)]
async fn tick_vault(
    cli: &Cli,
    cfg: &KeeperConfig,
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    oracle: &oracle_client::OracleClient,
    indexer: &IndexerClient,
    pyth_handles: &PythHandles,
    meta: &DiscoveredVault,
    ids: TickCtx<'_>,
) -> Result<()> {
    let now = now_ms();
    let view = fetch_vault_view(&wrap.client, meta.vault_id).await?;

    let lookback = view.config.round_ms.saturating_mul(RFQ_LOOKBACK_ROUNDS);
    let cutoff = now.saturating_sub(lookback);
    // One AuctionCreated walk covers both kinds; the escrow leg tells an
    // RFQ slice (Auction<U, S>) from a proceeds swap (Auction<S, U>).
    let (auctions, swap_auctions) = if view.open_rfqs > 0 || view.open_swap_rfqs > 0 {
        discover_open_auctions(
            &wrap.client,
            ids.auction_package,
            meta.vault_id,
            &meta.underlying_type,
            &meta.settlement_type,
            cutoff,
        )
        .await?
    } else {
        (Vec::new(), Vec::new())
    };

    // The current bucket's call type (and liveness) from the indexer.
    let bucket_meta = match view.current_bucket {
        Some(bucket_id) => Some(fetch_bucket_meta(indexer, meta, bucket_id).await?),
        None => None,
    };

    // Cap both the slice count and the stagger to this vault's selling window:
    // an hourly round's 30-min window runs a single slice, while a weekly
    // round's 12h window keeps the configured count (self-scaling, one config).
    let slices = keeper::slicing::effective_slices(
        ids.defaults.slicing.slices,
        view.config.selling_window_ms,
    );
    let stagger_ms = keeper::slicing::effective_stagger_ms(
        ids.defaults.slicing.stagger_minutes * 60_000,
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
    // Skip the steady-state Idle plan to avoid a per-vault log flood every tick;
    // only surface a plan when there's actually an action to take.
    if !matches!(action, Action::Idle) {
        debug!(vault = %meta.vault_id, round = view.round, ?action, "planned");
    }

    let ctx = SubmitCtx {
        wrap,
        http,
        hermes_url: &cfg.pyth.hermes_url,
        pyth: pyth_handles,
        package: ids.vault_package,
        protocol_config_id: ids.protocol_config_id,
        treasury_id: ids.treasury_id,
        vault_id: meta.vault_id,
        underlying_type: &meta.underlying_type,
        settlement_type: &meta.settlement_type,
        share_type: &meta.share_type,
        underlying_feed: meta.underlying_feed,
        settlement_feed: meta.settlement_feed,
        underlying_price_info: meta.underlying_price_info,
        settlement_price_info: meta.settlement_price_info,
        gas_budget: cli.gas_budget,
    };

    match action {
        Action::Idle => Ok(()),
        Action::SelectBucketNeeded => {
            select_bucket_or_finalize(cli, oracle, indexer, &ctx, meta, ids.defaults, &view, now)
                .await
        }
        other => {
            if cli.dry_run {
                info!(vault = %meta.vault_id, action = ?other, "dry-run: would submit");
                return Ok(());
            }
            execute(&ctx, &other).await
        }
    }
}

/// Resolve `SelectBucketNeeded`: σ + spot + candidates → pick →
/// `select_bucket`. No viable candidate: finalize if queued flows are
/// waiting on the round to roll, otherwise idle.
#[allow(clippy::too_many_arguments)]
async fn select_bucket_or_finalize(
    cli: &Cli,
    oracle: &oracle_client::OracleClient,
    indexer: &IndexerClient,
    ctx: &SubmitCtx<'_>,
    meta: &DiscoveredVault,
    defaults: &VaultDefaults,
    view: &VaultView,
    now: u64,
) -> Result<()> {
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

    // Skip rounds whose snapped strike can't clear the reserve the on-chain
    // open_rfq will set: the option's fair value caps any plausible bid, so a
    // sub-reserve strike can only churn open/settle gas on auctions that
    // always expire unsold. Treat it like no candidate — idle the round.
    match pick {
        Some(p) if p.clears_reserve(spot, view.config.min_reserve_premium_bps) => {
            // The calibration trail / forward-shadow dataset (doc 06 §9.4):
            // every selection logs (σ, K*, snapped strike, model delta).
            info!(
                vault = %ctx.vault_id,
                round = view.round,
                spot,
                sigma,
                sigma_iv,
                k_star = p.k_star_usd,
                strike = p.strike_usd,
                model_delta = p.model_delta,
                expiry_ms = p.expiry_ms,
                grid_coverage_miss = p.grid_coverage_miss,
                bucket = %p.bucket_id,
                "strike pick"
            );
            if p.grid_coverage_miss {
                warn!(vault = %ctx.vault_id, "GridCoverageMiss: no candidate ≥ K* — check the scheduler grid");
            }
            if cli.dry_run {
                info!(vault = %ctx.vault_id, "dry-run: would select_bucket");
                return Ok(());
            }
            execute_select_bucket(ctx, p.bucket_id, &p.call_type).await
        }
        Some(p) => {
            // A strike exists but its fair value can't clear the reserve —
            // skip the round rather than churn unsellable auctions. The
            // scheduler grid likely has no strike near K* at this tenor.
            info!(
                vault = %ctx.vault_id,
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
/// value clears the reserve. Finalize if deposits/withdrawals are queued and
/// waiting on the round to roll, otherwise idle.
async fn idle_or_finalize_round(cli: &Cli, ctx: &SubmitCtx<'_>, view: &VaultView) -> Result<()> {
    let flows_waiting = view.pending_deposits > 0 || view.queued_withdraw_shares > 0;
    if !flows_waiting {
        trace!(vault = %ctx.vault_id, "no viable strike and no queued flows; idling");
        return Ok(());
    }
    warn!(
        vault = %ctx.vault_id,
        round = view.round,
        "no viable strike but flows are queued — finalizing the idle round"
    );
    if cli.dry_run {
        info!(vault = %ctx.vault_id, "dry-run: would finalize_round (idle)");
        return Ok(());
    }
    execute(ctx, &Action::FinalizeRound).await
}

async fn fetch_bucket_meta(
    indexer: &IndexerClient,
    meta: &DiscoveredVault,
    bucket_id: ObjectID,
) -> Result<BucketMeta> {
    let buckets = indexer
        .buckets(
            false,
            // Indexer-form type strings: buckets and vaults are both
            // stored as chain TypeNames, so the exact-match filter holds
            // by construction (move-type-normalization.md).
            Some(&AssetType::new(meta.underlying_type_indexer.clone())),
            Some(&AssetType::new(meta.settlement_type_indexer.clone())),
            None,
        )
        .await
        .context("fetching buckets for the current round's bucket")?;
    let b = buckets
        .iter()
        .find(|b| ObjectID::new(*b.bucket_id.as_bytes()) == bucket_id)
        .ok_or_else(|| anyhow::anyhow!("current bucket {bucket_id} not in indexer — lagging?"))?;
    Ok(BucketMeta {
        call_type: b.call_type.to_canonical(),
        invalidated: b.invalidated,
    })
}

async fn fetch_candidates(
    indexer: &IndexerClient,
    meta: &DiscoveredVault,
) -> Result<Vec<BucketCandidate>> {
    let buckets = indexer
        .buckets(
            true,
            Some(&AssetType::new(meta.underlying_type_indexer.clone())),
            Some(&AssetType::new(meta.settlement_type_indexer.clone())),
            None,
        )
        .await
        .context("fetching candidate buckets")?;
    Ok(buckets
        .into_iter()
        .filter(|b| !b.invalidated && !b.cleaned)
        .map(|b| BucketCandidate {
            bucket_id: ObjectID::new(*b.bucket_id.as_bytes()),
            call_type: b.call_type.to_canonical(),
            strike_raw: b.strike,
            strike_scale: b.strike_scale,
            expiry_ms: b.expiry_ms,
        })
        .collect())
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
        return Err(anyhow::anyhow!("non-positive oracle prices: {u} / {s}"));
    }
    Ok(u / s)
}

/// Realized σ from oracle-service (cached/paced Benchmarks; it maps the beta
/// feed id to its stable Benchmarks id server-side), with the configured static
/// fallback for outages.
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

fn parse_id(s: &str, what: &str) -> Result<ObjectID> {
    ObjectID::from_str(s).with_context(|| format!("parsing {what} {s:?}"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
