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
//!      RfqCreated discovery), plan the single next action
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
use tracing::{debug, error, info, warn};

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
use keeper::state::{discover_open_rfqs, discover_open_swap_rfqs, fetch_vault_view, VaultView};
use keeper::strike::{pick_bucket, BucketCandidate};
use keeper::submit::{classify, execute, execute_select_bucket, ErrorClass, SubmitCtx};
use keeper::Cli;

/// How far back to scan RfqCreated events for live auctions: auctions
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

    let package = snapshot.package()?;
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
    // Cached, paced, bulk realized-vol against Pyth Benchmarks. Shared across
    // every vault and tick so each daily close is fetched once (immutable
    // history) and requests stay under the endpoint's rate cap — without it,
    // each `fetch_sigma` bursts `vol_window_days + 1` unpaced requests per
    // call and storms the endpoint with 429s. Reuses `http` so it carries the
    // same Pyth auth header.
    let bench = pyth_client::BenchmarkVol::new(http.clone(), cfg.pyth.benchmarks_url.clone());
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
                package,
                protocol_config_id,
                treasury_id,
                defaults: &cfg.vault_defaults,
            };
            match tick_vault(&cli, &cfg, &wrap, &http, &bench, &indexer, &pyth_handles, meta, ctx).await {
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
    package: ObjectID,
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
    bench: &pyth_client::BenchmarkVol,
    indexer: &IndexerClient,
    pyth_handles: &PythHandles,
    meta: &DiscoveredVault,
    ids: TickCtx<'_>,
) -> Result<()> {
    let now = now_ms();
    let view = fetch_vault_view(&wrap.client, meta.vault_id).await?;

    let lookback = view.config.round_ms.saturating_mul(RFQ_LOOKBACK_ROUNDS);
    let cutoff = now.saturating_sub(lookback);
    let auctions = if view.open_rfqs > 0 {
        discover_open_rfqs(&wrap.client, ids.package, meta.vault_id, cutoff).await?
    } else {
        Vec::new()
    };
    let swap_auctions = if view.open_swap_rfqs > 0 {
        discover_open_swap_rfqs(&wrap.client, ids.package, meta.vault_id, cutoff).await?
    } else {
        Vec::new()
    };

    // The current bucket's call type (and liveness) from the indexer.
    let bucket_meta = match view.current_bucket {
        Some(bucket_id) => Some(fetch_bucket_meta(indexer, meta, bucket_id).await?),
        None => None,
    };

    // Cap the configured stagger to this vault's selling window so an hourly
    // round's short window still opens multiple slices (weekly unchanged).
    let stagger_ms = keeper::slicing::effective_stagger_ms(
        ids.defaults.slicing.stagger_minutes * 60_000,
        view.config.selling_window_ms,
        ids.defaults.slicing.slices,
    );

    let action = plan(&PlanInput {
        view: &view,
        now_ms: now,
        auctions: &auctions,
        swap_auctions: &swap_auctions,
        bucket_meta: bucket_meta.as_ref(),
        stagger_ms,
        max_slices: ids.defaults.slicing.slices,
    });
    debug!(vault = %meta.vault_id, round = view.round, ?action, "planned");

    let ctx = SubmitCtx {
        wrap,
        http,
        hermes_url: &cfg.pyth.hermes_url,
        pyth: pyth_handles,
        package: ids.package,
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
            select_bucket_or_finalize(
                cli, cfg, http, bench, indexer, &ctx, meta, ids.defaults, &view, now,
            )
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
    cfg: &KeeperConfig,
    http: &reqwest::Client,
    bench: &pyth_client::BenchmarkVol,
    indexer: &IndexerClient,
    ctx: &SubmitCtx<'_>,
    meta: &DiscoveredVault,
    defaults: &VaultDefaults,
    view: &VaultView,
    now: u64,
) -> Result<()> {
    let candidates = fetch_candidates(indexer, meta).await?;
    let spot = fetch_spot_cross(http, cfg, meta).await?;
    let sigma = fetch_sigma(bench, meta, defaults, now).await?;
    let sigma_iv = sigma * defaults.iv_ratio;

    let pick = pick_bucket(
        &candidates,
        spot,
        sigma_iv,
        now,
        &view.config,
        meta.underlying_decimals,
        meta.settlement_decimals,
        defaults.target_delta,
    );

    match pick {
        Some(p) => {
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
        None => {
            let flows_waiting = view.pending_deposits > 0 || view.queued_withdraw_shares > 0;
            if !flows_waiting {
                debug!(vault = %ctx.vault_id, "no selectable bucket and no queued flows; idling");
                return Ok(());
            }
            warn!(
                vault = %ctx.vault_id,
                round = view.round,
                "no selectable bucket but flows are queued — finalizing the idle round"
            );
            if cli.dry_run {
                info!(vault = %ctx.vault_id, "dry-run: would finalize_round (idle)");
                return Ok(());
            }
            execute(ctx, &Action::FinalizeRound).await
        }
    }
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

/// USD cross (settlement-per-underlying) from the two Hermes feeds.
async fn fetch_spot_cross(
    http: &reqwest::Client,
    cfg: &KeeperConfig,
    meta: &DiscoveredVault,
) -> Result<f64> {
    let updates = pyth_client::latest(
        http,
        &cfg.pyth.hermes_url,
        &[meta.underlying_feed, meta.settlement_feed],
    )
    .await
    .context("fetching hermes spot")?;
    let mut u = None;
    let mut s = None;
    for upd in &updates {
        let feed = upd.feed_id()?;
        if feed == meta.underlying_feed {
            u = Some(upd.price.price_f64()?);
        }
        if feed == meta.settlement_feed {
            s = Some(upd.price.price_f64()?);
        }
    }
    let (u, s) = (
        u.ok_or_else(|| anyhow::anyhow!("hermes returned no underlying price"))?,
        s.ok_or_else(|| anyhow::anyhow!("hermes returned no settlement price"))?,
    );
    if !(u > 0.0 && s > 0.0) {
        return Err(anyhow::anyhow!("non-positive hermes prices: {u} / {s}"));
    }
    Ok(u / s)
}

/// Realized σ from Pyth Benchmarks (README §9), with the configured
/// static fallback for outages (and for beta feed ids, which Benchmarks
/// never serves).
async fn fetch_sigma(
    bench: &pyth_client::BenchmarkVol,
    meta: &DiscoveredVault,
    defaults: &VaultDefaults,
    now: u64,
) -> Result<f64> {
    match bench
        .realized_sigma(
            // The discovered feed is a beta id; Benchmarks only serves stable,
            // so map it across. Unmapped ids pass through and fall back as
            // before.
            pyth_client::benchmark_feed_id(meta.underlying_feed),
            defaults.vol_window_days,
            (now / 1000) as i64,
        )
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
