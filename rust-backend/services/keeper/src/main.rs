//! vault-keeper — permissionless crank-driver for the covered-call
//! vaults (`services/keeper/README.md` is the build spec).
//!
//! Boot:
//!   1. Parse Cli, load config + secrets, fetch the token-info snapshot
//!      (protocol ids, coin types/decimals, Pyth feeds, DEEP type).
//!   2. Connect SuiClient. Any funded wallet works — the keeper holds no
//!      capability objects; `vault.move` validates every crank.
//!   3. Resolve each configured vault's types, feeds, and price-info ids.
//!
//! Tick loop (every `tick_secs`, default 15): per vault, read the chain
//! ([`state::fetch_vault_view`] + RfqCreated discovery), plan the single
//! next action ([`planner::plan`]), resolve a strike pick when asked,
//! and submit with a Pyth price update prepended in-PTB ([`submit`]).
//! Fatal errors (config bugs) halt that vault until restart; everything
//! else replans from fresh state next tick.

use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use indexer_graphql::IndexerClient;
use protocol_types::asset::AssetType;
use pyth_client::types::PriceFeedId;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::pyth_update::PythHandles;
use sui_types::base_types::ObjectID;
use token_info_client::TokenInfoClient;

use keeper::config::{KeeperConfig, VaultEntry};
use keeper::planner::{plan, Action, BucketMeta, PlanInput};
use keeper::state::{discover_open_rfqs, fetch_vault_view, VaultView};
use keeper::strike::{pick_bucket, BucketCandidate};
use keeper::submit::{classify, execute, execute_select_bucket, ErrorClass, SubmitCtx};
use keeper::Cli;

/// How far back to scan RfqCreated events for live auctions: auctions
/// can't outlive a round, so two round lengths is generous.
const RFQ_LOOKBACK_ROUNDS: u64 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = KeeperConfig::load(&cli.config)
        .with_context(|| format!("loading {}", cli.config.display()))?;
    runtime_config::health::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    let package = snapshot.package()?;
    let protocol_config_id = snapshot.protocol_config()?;
    let treasury_id = snapshot.treasury()?;
    let deep_coin_type = snapshot.deepbook().map(|db| db.deep_coin_type.clone());
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    info!(signer = %wrap.signer.address, "keeper wallet connected (gas only)");

    if cfg.vaults.is_empty() {
        // Deploy wiring ships before any vault object exists on chain
        // (mirrors the scheduler's empty prod [[pairs]]): boot, keep
        // /health up, and idle until a config with [[vaults]] is rolled.
        warn!(
            "no [[vaults]] configured in {} — idling (health endpoint stays up)",
            cli.config.display()
        );
    }

    let pyth_handles = PythHandles {
        pyth_package: parse_id(&cfg.pyth.pyth_package_id, "pyth_package_id")?,
        wormhole_package: parse_id(&cfg.pyth.wormhole_package_id, "wormhole_package_id")?,
        pyth_state_id: parse_id(&cfg.pyth.pyth_state_id, "pyth_state_id")?,
        wormhole_state_id: parse_id(&cfg.pyth.wormhole_state_id, "wormhole_state_id")?,
        update_fee_mist: cfg.pyth.update_fee_mist,
    };

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;
    let indexer = IndexerClient::new(cfg.indexer_graphql_url.clone());

    let mut vaults: Vec<VaultMeta> = Vec::with_capacity(cfg.vaults.len());
    for entry in &cfg.vaults {
        vaults.push(VaultMeta::resolve(entry, &snapshot)?);
    }
    for v in &vaults {
        info!(
            vault = %v.vault_id,
            pair = %format!("{}/{}", v.entry.underlying, v.entry.settlement),
            iv_ratio = v.entry.iv_ratio,
            "vault configured"
        );
    }

    // Vaults halted on a Fatal classification; cleared only by restart.
    let mut halted: HashSet<ObjectID> = HashSet::new();

    let tick = Duration::from_secs(cfg.tick_secs.max(1));
    info!(tick_secs = cfg.tick_secs, dry_run = cli.dry_run, "tick loop starting");
    loop {
        for meta in &vaults {
            if halted.contains(&meta.vault_id) {
                continue;
            }
            match tick_vault(&cli, &cfg, &wrap, &http, &indexer, &pyth_handles, meta, TickIds {
                package,
                protocol_config_id,
                treasury_id,
                deep_coin_type: deep_coin_type.as_deref(),
            })
            .await
            {
                Ok(()) => {}
                Err(e) => match classify(&e) {
                    ErrorClass::Benign => {
                        debug!(vault = %meta.vault_id, error = %format!("{e:#}"), "lost a race; replanning next tick");
                    }
                    ErrorClass::Retry => {
                        warn!(vault = %meta.vault_id, error = %format!("{e:#}"), "transient failure; retrying next tick");
                    }
                    ErrorClass::Fatal => {
                        error!(vault = %meta.vault_id, error = %format!("{e:#}"), "FATAL: halting this vault until restart");
                        halted.insert(meta.vault_id);
                    }
                },
            }
        }
        sleep(tick).await;
    }
}

/// Per-tick protocol-level ids (resolved once at boot).
#[derive(Clone, Copy)]
struct TickIds<'a> {
    package: ObjectID,
    protocol_config_id: ObjectID,
    treasury_id: ObjectID,
    deep_coin_type: Option<&'a str>,
}

/// One configured vault, resolved against the token-info catalog.
struct VaultMeta {
    entry: VaultEntry,
    vault_id: ObjectID,
    underlying_type: String,
    settlement_type: String,
    underlying_decimals: u8,
    settlement_decimals: u8,
    underlying_feed: PriceFeedId,
    settlement_feed: PriceFeedId,
    underlying_price_info: ObjectID,
    settlement_price_info: ObjectID,
    deep_funding: Option<(ObjectID, u64)>,
}

impl VaultMeta {
    fn resolve(entry: &VaultEntry, snapshot: &token_info_client::Snapshot) -> Result<Self> {
        let u_spec = snapshot
            .token_spec(&entry.underlying)
            .with_context(|| format!("underlying {} not in token-info catalog", entry.underlying))?;
        let s_spec = snapshot
            .token_spec(&entry.settlement)
            .with_context(|| format!("settlement {} not in token-info catalog", entry.settlement))?;
        let deep_funding = match (&entry.deep_funding_coin, entry.deep_fee_per_swap) {
            (Some(coin), Some(amount)) => Some((parse_id(coin, "deep_funding_coin")?, amount)),
            (None, None) => None,
            _ => {
                return Err(anyhow!(
                    "deep_funding_coin and deep_fee_per_swap must be set together"
                ))
            }
        };
        Ok(Self {
            entry: entry.clone(),
            vault_id: parse_id(&entry.vault_id, "vault_id")?,
            underlying_type: u_spec.coin_type.clone(),
            settlement_type: s_spec.coin_type.clone(),
            underlying_decimals: u_spec.decimals,
            settlement_decimals: s_spec.decimals,
            underlying_feed: u_spec.pyth_feed()?,
            settlement_feed: s_spec.pyth_feed()?,
            underlying_price_info: parse_id(&entry.underlying_price_info, "underlying_price_info")?,
            settlement_price_info: parse_id(&entry.settlement_price_info, "settlement_price_info")?,
            deep_funding,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn tick_vault(
    cli: &Cli,
    cfg: &KeeperConfig,
    wrap: &SuiClientWrapper,
    http: &reqwest::Client,
    indexer: &IndexerClient,
    pyth_handles: &PythHandles,
    meta: &VaultMeta,
    ids: TickIds<'_>,
) -> Result<()> {
    let now = now_ms();
    let view = fetch_vault_view(&wrap.client, meta.vault_id).await?;

    let lookback = view.config.round_ms.saturating_mul(RFQ_LOOKBACK_ROUNDS);
    let auctions = if view.open_rfqs > 0 {
        discover_open_rfqs(&wrap.client, ids.package, meta.vault_id, now.saturating_sub(lookback))
            .await?
    } else {
        Vec::new()
    };

    // The current bucket's call type (and liveness) from the indexer.
    let bucket_meta = match view.current_bucket {
        Some(bucket_id) => Some(fetch_bucket_meta(indexer, meta, bucket_id).await?),
        None => None,
    };

    let action = plan(&PlanInput {
        view: &view,
        now_ms: now,
        auctions: &auctions,
        bucket_meta: bucket_meta.as_ref(),
        stagger_ms: meta.entry.slicing.stagger_minutes * 60_000,
        max_slices: meta.entry.slicing.slices,
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
        share_type: &meta.entry.share_type,
        underlying_feed: meta.underlying_feed,
        settlement_feed: meta.settlement_feed,
        underlying_price_info: meta.underlying_price_info,
        settlement_price_info: meta.settlement_price_info,
        deepbook_pool_id: view.config.deepbook_pool_id,
        deep_coin_type: ids.deep_coin_type,
        deep_funding: meta.deep_funding,
        gas_budget: cli.gas_budget,
    };

    match action {
        Action::Idle => Ok(()),
        Action::SelectBucketNeeded => {
            select_bucket_or_finalize(cli, cfg, http, indexer, &ctx, meta, &view, now).await
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
    indexer: &IndexerClient,
    ctx: &SubmitCtx<'_>,
    meta: &VaultMeta,
    view: &VaultView,
    now: u64,
) -> Result<()> {
    let candidates = fetch_candidates(indexer, meta).await?;
    let spot = fetch_spot_cross(http, cfg, meta).await?;
    let sigma = fetch_sigma(http, cfg, meta, now).await?;
    let sigma_iv = sigma * meta.entry.iv_ratio;

    let pick = pick_bucket(
        &candidates,
        spot,
        sigma_iv,
        now,
        &view.config,
        meta.underlying_decimals,
        meta.settlement_decimals,
        meta.entry.target_delta,
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
    meta: &VaultMeta,
    bucket_id: ObjectID,
) -> Result<BucketMeta> {
    let buckets = indexer
        .buckets(
            false,
            Some(&AssetType::new(meta.underlying_type.clone())),
            Some(&AssetType::new(meta.settlement_type.clone())),
            None,
        )
        .await
        .context("fetching buckets for the current round's bucket")?;
    let b = buckets
        .iter()
        .find(|b| ObjectID::new(*b.bucket_id.as_bytes()) == bucket_id)
        .ok_or_else(|| anyhow!("current bucket {bucket_id} not in indexer — lagging?"))?;
    Ok(BucketMeta {
        call_type: b.call_type.as_str().to_string(),
        invalidated: b.invalidated,
    })
}

async fn fetch_candidates(
    indexer: &IndexerClient,
    meta: &VaultMeta,
) -> Result<Vec<BucketCandidate>> {
    let buckets = indexer
        .buckets(
            true,
            Some(&AssetType::new(meta.underlying_type.clone())),
            Some(&AssetType::new(meta.settlement_type.clone())),
            None,
        )
        .await
        .context("fetching candidate buckets")?;
    Ok(buckets
        .into_iter()
        .filter(|b| !b.invalidated && !b.cleaned)
        .map(|b| BucketCandidate {
            bucket_id: ObjectID::new(*b.bucket_id.as_bytes()),
            call_type: b.call_type.as_str().to_string(),
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
    meta: &VaultMeta,
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
        u.ok_or_else(|| anyhow!("hermes returned no underlying price"))?,
        s.ok_or_else(|| anyhow!("hermes returned no settlement price"))?,
    );
    if !(u > 0.0 && s > 0.0) {
        return Err(anyhow!("non-positive hermes prices: {u} / {s}"));
    }
    Ok(u / s)
}

/// Realized σ from Pyth Benchmarks (README §9), with the configured
/// static fallback for outages.
async fn fetch_sigma(
    http: &reqwest::Client,
    cfg: &KeeperConfig,
    meta: &VaultMeta,
    now: u64,
) -> Result<f64> {
    match pyth_client::sigma::realized_sigma_from_benchmarks(
        http,
        &cfg.pyth.benchmarks_url,
        meta.underlying_feed,
        meta.entry.vol_window_days,
        (now / 1000) as i64,
    )
    .await
    {
        Ok(s) => Ok(s),
        Err(e) => match meta.entry.sigma_fallback {
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
