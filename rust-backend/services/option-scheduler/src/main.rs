//! option-scheduler — bucket-creation lifecycle bot.
//!
//! Boot:
//!   1. Parse Cli + load deployments + secrets.
//!   2. Connect SuiClient; assert signer == deployer (only address with AdminCap).
//!   3. For each configured pair: resolve underlying/settlement `TokenInfo`,
//!      build a canonicalised PairKey.
//!   4. Spawn the indexer subscriber. It hydrates the in-memory family
//!      registry from the snapshot and follows the live stream forever.
//!
//! Tick loop (every `tick_secs`, default 60):
//!   For each pair, find the family with the latest expiry. If
//!   `latest_expiry - now < roll_threshold_ms`, compute the next expiry
//!   via the cadence and the strike grid via current spot, then submit
//!   `bucket::new_call_option<U, S>` (or just log under --dry-run).
//!
//! Updates to the registry never happen from inside this binary — every
//! BucketCreated we ever care about comes back through the indexer, so
//! the registry stays single-sourced.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::time::sleep;
use tracing::{error, info, warn};

use shared::deployments::Deployments;
use shared::sui_client::SuiClientWrapper;

use option_scheduler::config::{PairConfig, SchedulerConfig};
use option_scheduler::families::{CanonicalType, PairKey, Registry, log_registry, run_subscriber};
use option_scheduler::roller::{self, RollPlan};
use option_scheduler::schedule::next_expiry_ms;
use option_scheduler::spot::ResolvedSpotSource;
use option_scheduler::strike_grid::build_strike_grid_from_chain;
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
    let secrets = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network.as_str())?;

    let package = net.package()?;
    let admin_cap = net.admin_cap()?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;

    // AdminCap belongs to the deployer only — exchange enforces the same check
    // (tools/exchange/src/main.rs). A scheduler signed by anyone else is
    // useless and we'd rather fail loudly at boot than hit a chain revert on
    // every tick.
    let deployer = net.deployer_address()?;
    if wrap.signer.address != deployer {
        return Err(anyhow!(
            "configured signer {} ≠ deployer {} from deployments.json — \
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
        let u = net.token(&pair.underlying).with_context(|| {
            format!(
                "underlying {} not in deployments.testTokens",
                pair.underlying
            )
        })?;
        let s = net.token(&pair.settlement).with_context(|| {
            format!(
                "settlement {} not in deployments.testTokens",
                pair.settlement
            )
        })?;
        let u_spec = net.token_spec(&pair.underlying).with_context(|| {
            format!(
                "underlying {} not in deployments.token_info",
                pair.underlying
            )
        })?;
        let s_spec = net.token_spec(&pair.settlement).with_context(|| {
            format!(
                "settlement {} not in deployments.token_info",
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

    // Indexer subscriber — runs forever in the background.
    {
        let url = cfg.indexer_url.clone();
        let registry = registry.clone();
        let pair_keys = pair_keys.clone();
        tokio::spawn(async move {
            if let Err(e) = run_subscriber(url, registry, pair_keys).await {
                error!(error = %e, "indexer subscriber exited");
            }
        });
    }

    // Give the snapshot a moment to land before the first tick so the
    // dry-run print isn't empty. Anything we miss now will be filled in
    // on the next tick.
    sleep(Duration::from_secs(2)).await;
    log_registry(&registry, &pair_keys);

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
) -> Result<()> {
    let now = now_ms();
    for (idx, meta) in pairs.iter().enumerate() {
        let pair_label = format!("{}/{}", meta.cfg.underlying, meta.cfg.settlement);
        let latest = registry.latest_family(idx);
        let latest_expiry = latest.as_ref().map(|f| f.expiry_ms);

        // Are we inside the roll window?
        let needs_roll = match latest_expiry {
            None => true, // no family on chain yet — cold-start roll
            Some(t) => t.saturating_sub(now) < cfg.roll_threshold_ms,
        };
        if !needs_roll {
            continue;
        }

        let next_expiry = next_expiry_ms(latest_expiry, meta.cfg.expiry_interval_ms, now);
        // Sanity: don't roll if we already have a family at this exact
        // expiry (indexer may have lagged behind a previous tick's submit).
        if let Some(t) = latest_expiry {
            if t >= next_expiry {
                continue;
            }
        }

        let spot_chain = match meta
            .spot
            .resolve_chain_units(http_client, &cfg.pyth.hermes_url)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "spot resolve failed; skipping");
                continue;
            }
        };
        let grid = match build_strike_grid_from_chain(
            spot_chain,
            meta.cfg.strikes_below,
            meta.cfg.strikes_above,
            meta.cfg.interval_pct,
        ) {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, pair = %pair_label, "strike grid invalid; skipping");
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
            continue;
        }
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
            }
            Err(e) => {
                warn!(error = %e, "new_call_option submit failed");
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

