//! staging-mm-bot — faucet-funded maker bot for the hybrid exchange (SO-368).
//!
//! Boot: config + secrets (hard-gated to testnet), token-info catalog,
//! orderbook market discovery, oracle-service WS price cache, then a
//! BalanceManager (adopt-or-create) funded from the test-token faucets.
//!
//! Steady state, per market: read the oracle mid, build a tick-snapped
//! bid/ask ladder, sign each level with the wallet key, post to the
//! orderbook; on drift or approaching expiry, soft-cancel and repost, then
//! queue an on-chain salt-watermark raise (`cancel_up_to`) that an hourly
//! sweep submits for all markets in one PTB — the short order TTL bounds
//! how long a replaced order stays fillable, so the watermark is a slow
//! belt, not the primary cancel. A funding task keeps per-token escrow at
//! its configured float.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use serde::Deserialize;
use sui_types::base_types::ObjectID;

use exchange_types::{canonicalize_move_type, Digest, Market, SuiAddress as ExAddress};
use pyth_client::{compute_spot_from_cache, PriceCache, PriceFeedId, Staleness};
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::exchange as exchange_tx;
use token_info_client::TokenInfoClient;

use staging_mm_bot::client::{IntakeReject, OrderbookClient};
use staging_mm_bot::ladder::{self, LevelSpec};
use staging_mm_bot::signing::OrderSigner;
use staging_mm_bot::{server, Cli};

// -- Config --------------------------------------------------------------

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:8085".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
struct BotConfig {
    /// HTTP health-check bind address. Defaults to `0.0.0.0:8085`.
    #[serde(default = "default_health_addr")]
    health_addr: std::net::SocketAddr,

    /// Sui network. This bot free-mints from faucets, so anything other
    /// than testnet is rejected at load.
    network: Network,

    /// Per-order signed fee ceiling. Chain caps at 50; market default is 10.
    #[serde(default = "default_max_fee_bps")]
    max_fee_bps: u64,

    #[serde(default)]
    funding: FundingConfig,

    #[serde(default)]
    quoting: QuotingConfig,

    /// Price staleness gates applied to the oracle WS cache.
    #[serde(default)]
    pyth: PythConfig,
}

fn default_max_fee_bps() -> u64 {
    50
}

impl BotConfig {
    fn validate(&self) -> Result<()> {
        if self.network != Network::Testnet {
            bail!(
                "network = {} — staging-mm-bot free-mints from the test-token faucets and \
                 must never run off testnet",
                self.network
            );
        }
        if self.quoting.levels.is_empty() {
            bail!("[quoting].levels is empty — nothing to quote");
        }
        if self.quoting.ttl_secs < 60 {
            // Intake's floor is 30s; below ~60s the requote loop churns.
            bail!("[quoting].ttl_secs must be >= 60");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct FundingConfig {
    enabled: bool,
    check_interval_secs: u64,
    /// Ticker → escrow float target in the token's raw units. Topped back
    /// up to target when the mirrored balance falls below half of it.
    targets: HashMap<String, u64>,
}

impl Default for FundingConfig {
    fn default() -> Self {
        Self { enabled: true, check_interval_secs: 60, targets: HashMap::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct QuotingConfig {
    /// Ladder levels, inner first: half-spread bps from mid + size in lots
    /// (1 lot = one whole base token).
    levels: Vec<LevelSpec>,
    /// Order lifetime. Intake floor is 30s.
    ttl_secs: u64,
    /// Cadence of the per-market quote check.
    refresh_secs: u64,
    /// Requote when the mid drifts this many bps from the quoted mid.
    requote_drift_bps: u64,
    /// Fraction of mirrored escrow the ladder may commit per token.
    escrow_utilization: f64,
    /// Cadence of the batched on-chain watermark sweep. Safe to keep slow:
    /// replaced orders expire on-chain after `ttl_secs` regardless.
    watermark_interval_secs: u64,
}

impl Default for QuotingConfig {
    fn default() -> Self {
        Self {
            levels: vec![
                LevelSpec { bps: 10, lots: 1 },
                LevelSpec { bps: 25, lots: 2 },
                LevelSpec { bps: 50, lots: 4 },
            ],
            ttl_secs: 90,
            refresh_secs: 5,
            requote_drift_bps: 5,
            escrow_utilization: 0.8,
            watermark_interval_secs: 3_600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PythConfig {
    max_price_age_ms: u64,
    max_publish_lag_ms: u64,
    max_conf_bps: u64,
}

impl Default for PythConfig {
    fn default() -> Self {
        Self { max_price_age_ms: 5_000, max_publish_lag_ms: 10_000, max_conf_bps: 0 }
    }
}

fn load_config(path: &Path) -> Result<BotConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let cfg: BotConfig =
        toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// Strictly-increasing salt source: time-seeded so a restart resumes above
/// everything a previous run issued, atomic so concurrent market tasks never
/// collide. Salts are per-(maker, registry) monotonic forever on the books.
struct SaltSource(AtomicU64);

impl SaltSource {
    fn new() -> Self {
        Self(AtomicU64::new(now_ms() * 1_000))
    }
    fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

// -- Market context ------------------------------------------------------

/// Everything a per-market quoter needs, resolved once at boot.
struct MarketCtx {
    market: Market,
    registry: ObjectID,
    base_feed: PriceFeedId,
    quote_feed: PriceFeedId,
    base_decimals: u8,
    quote_decimals: u8,
}

/// Per-market on-chain watermark bookkeeping for the batched hourly sweep.
struct MarketWatermark {
    base: String,
    quote: String,
    /// Highest salt queued for the next sweep, if any.
    pending: Option<u64>,
    /// Highest watermark we know landed on-chain (reactive or swept). The
    /// sweep never submits at or below this — the contract aborts with
    /// `EWatermarkRegression` on a lower value, which would fail the whole
    /// batch PTB.
    raised: u64,
}

/// Shared handles across tasks.
struct Shared {
    wrap: SuiClientWrapper,
    ob: OrderbookClient,
    signer: OrderSigner,
    manager: ExAddress,
    manager_oid: ObjectID,
    exchange_package: ObjectID,
    salts: SaltSource,
    cfg: BotConfig,
    gas_budget: u64,
    cache: PriceCache,
    staleness: Staleness,
    watermarks: Mutex<HashMap<ObjectID, MarketWatermark>>,
}

// -- Main ----------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("staging-mm-bot");

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    let readiness = observability::ops::Readiness::new();
    server::spawn(cfg.health_addr, readiness.clone());

    let wrap = SuiClientWrapper::connect(&secrets, cfg.network).await?;
    let signer = OrderSigner::from_sui_bech32(&secrets.sui_private_key(cfg.network.as_str())?)?;
    // Same key everywhere: the exchange-signing address derivation must land
    // on the wallet address or something is deeply wrong.
    if signer.address().to_hex() != wrap.signer.address.to_string() {
        bail!(
            "order-signer address {} != wallet address {}",
            signer.address().to_hex(),
            wrap.signer.address
        );
    }

    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    // Orderbook market discovery. Registry ids are the order-signature
    // domain, so they are only ever read from here — never from config.
    let ob = OrderbookClient::new(&cli.orderbook_url);
    let markets_resp = wait_for_markets(&ob, 60, Duration::from_secs(5)).await?;
    let exchange_package = ObjectID::from_str(&markets_resp.package_id)
        .context("parsing exchange packageId from /v1/markets")?;

    let mut market_ctxs = Vec::new();
    for m in markets_resp.markets {
        match market_ctx(m, &snapshot) {
            Ok(ctx) => market_ctxs.push(ctx),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "skipping market"),
        }
    }
    if market_ctxs.is_empty() {
        bail!("no quotable markets (orderbook served none the token catalog can price)");
    }
    tracing::info!(
        markets = ?market_ctxs.iter().map(|c| c.market.symbol.clone()).collect::<Vec<_>>(),
        %exchange_package,
        "markets resolved"
    );

    // Live prices from oracle-service over its WS fanout.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    let (cache, _ws_task) = oracle.subscribe();
    let mut feeds: Vec<PriceFeedId> =
        market_ctxs.iter().flat_map(|c| [c.base_feed, c.quote_feed]).collect();
    feeds.dedup();
    wait_for_first_prices(&cache, &feeds, Duration::from_secs(30)).await?;

    // BalanceManager: rediscover the one this wallet already created under
    // the CURRENT exchange package, else create one. Zero config: a
    // contract redeploy changes the package (and therefore the manager
    // type), so discovery scopes itself to the live deployment and a fresh
    // manager appears on the first boot after each redeploy.
    let manager_oid = resolve_balance_manager(&wrap, exchange_package, cli.gas_budget).await?;
    let manager = ExAddress::parse(&manager_oid.to_string())
        .map_err(|e| anyhow!("manager id hex: {e}"))?;
    tracing::info!(manager = %manager_oid, "BalanceManager ready");

    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
        max_conf_bps: cfg.pyth.max_conf_bps,
    };
    let shared = Arc::new(Shared {
        wrap,
        ob,
        signer,
        manager,
        manager_oid,
        exchange_package,
        salts: SaltSource::new(),
        cfg,
        gas_budget: cli.gas_budget,
        cache,
        staleness,
        watermarks: Mutex::new(HashMap::new()),
    });

    // Seed escrow before quoting so the first ladder doesn't bounce off
    // INSUFFICIENT_ESCROW, then keep it topped up in the background.
    if shared.cfg.funding.enabled {
        funding_pass(&shared, &snapshot).await;
        let s = Arc::clone(&shared);
        let snap = snapshot.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(Duration::from_secs(s.cfg.funding.check_interval_secs));
            tick.tick().await; // immediate first tick already ran above
            loop {
                tick.tick().await;
                funding_pass(&s, &snap).await;
            }
        });
    }

    // Batched watermark sweep: one PTB per interval voids every replaced
    // batch across all markets, instead of one tx per requote per market.
    {
        let s = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                s.cfg.quoting.watermark_interval_secs,
            ));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // fires immediately; nothing pending yet
            loop {
                tick.tick().await;
                watermark_sweep(&s).await;
            }
        });
    }

    readiness.ready();
    tracing::info!("staging-mm-bot ready; starting quoters");

    let mut handles = Vec::new();
    for ctx in market_ctxs {
        let s = Arc::clone(&shared);
        handles.push(tokio::spawn(async move { quote_loop(s, ctx).await }));
    }
    for h in handles {
        h.await.ok();
    }
    Ok(())
}

fn market_ctx(market: Market, snapshot: &token_info_client::Snapshot) -> Result<MarketCtx> {
    let base_spec = snapshot
        .token_by_coin_type(&market.base)
        .ok_or_else(|| anyhow!("{}: base {} not in token catalog", market.symbol, market.base))?;
    let quote_spec = snapshot
        .token_by_coin_type(&market.quote)
        .ok_or_else(|| anyhow!("{}: quote {} not in token catalog", market.symbol, market.quote))?;
    let registry = ObjectID::from_str(&market.registry_id.to_hex())
        .context("parsing market registry id")?;
    Ok(MarketCtx {
        base_feed: base_spec.pyth_feed()?,
        quote_feed: quote_spec.pyth_feed()?,
        base_decimals: base_spec.decimals,
        quote_decimals: quote_spec.decimals,
        registry,
        market,
    })
}

async fn wait_for_markets(
    ob: &OrderbookClient,
    attempts: u32,
    delay: Duration,
) -> Result<staging_mm_bot::client::MarketsResponse> {
    for attempt in 1..=attempts {
        match ob.markets().await {
            Ok(resp) if !resp.markets.is_empty() => return Ok(resp),
            Ok(_) => tracing::warn!(attempt, "orderbook serves no markets yet"),
            Err(e) => tracing::warn!(attempt, error = %format!("{e:#}"), "orderbook not reachable"),
        }
        tokio::time::sleep(delay).await;
    }
    bail!("orderbook served no markets after {attempts} attempts")
}

async fn wait_for_first_prices(
    cache: &PriceCache,
    feeds: &[PriceFeedId],
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let missing: Vec<_> = feeds.iter().filter(|f| cache.peek(**f).is_none()).collect();
        if missing.is_empty() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            bail!("no price for {} feed(s) after {timeout:?}", missing.len());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// -- BalanceManager provisioning -----------------------------------------

/// Rediscover this wallet's BalanceManager under the current exchange
/// package, or create one.
///
/// `balance_manager::new` emits nothing, but the bot's funding pass
/// deposits immediately after creating, so the manager reappears through
/// its `DepositEvent`s (`owner` = manager owner). The event type is scoped
/// to the live package, which is what makes this survive contract
/// redeployments with zero config: a new package means no events, and a
/// fresh manager is created for it. A discovered id is verified on-chain
/// (type + owner) before adoption — worst case (pruned event history, bad
/// candidate) we create a new manager; the old one stays owner-withdrawable.
async fn resolve_balance_manager(
    wrap: &SuiClientWrapper,
    exchange_package: ObjectID,
    gas_budget: u64,
) -> Result<ObjectID> {
    let event_type = format!("{exchange_package}::balance_manager::DepositEvent");
    let our_address = wrap.signer.address.to_string().to_lowercase();
    let mut cursor: Option<String> = None;
    // Newest first; 10 pages of 50 is far deeper than the funding cadence
    // needs — the latest top-up is never more than an hour old in steady
    // state.
    for _ in 0..10 {
        let page = match wrap
            .events
            .query_by_type(&event_type, cursor.as_deref(), 50, true)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "DepositEvent query failed; creating a manager");
                break;
            }
        };
        for ev in &page.data {
            let owner = ev.parsed_json.get("owner").and_then(|v| v.as_str());
            if owner.map(str::to_lowercase).as_deref() != Some(our_address.as_str()) {
                continue;
            }
            let Some(raw) = ev.parsed_json.get("manager").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(id) = ObjectID::from_str(raw) else { continue };
            match verify_manager(wrap, id).await {
                Ok(()) => {
                    tracing::info!(manager = %id, "adopted existing BalanceManager");
                    return Ok(id);
                }
                Err(e) => {
                    tracing::warn!(manager = %id, error = %format!("{e:#}"), "candidate manager rejected");
                }
            }
        }
        if !page.has_next_page || page.next_cursor.is_none() {
            break;
        }
        cursor = page.next_cursor;
    }
    exchange_tx::create_balance_manager(&wrap.client, &wrap.signer, exchange_package, gas_budget)
        .await
}

async fn verify_manager(wrap: &SuiClientWrapper, id: ObjectID) -> Result<()> {
    let (obj, json) = wrap
        .client
        .get_object_json(id)
        .await
        .with_context(|| format!("reading BalanceManager {id}"))?;
    let type_ok = obj
        .type_()
        .map(|t| t.to_string().ends_with("::balance_manager::BalanceManager"))
        .unwrap_or(false);
    if !type_ok {
        bail!("{id} is not a BalanceManager");
    }
    let owner = json
        .as_ref()
        .and_then(|j| j.get("owner"))
        .and_then(|o| o.as_str())
        .ok_or_else(|| anyhow!("BalanceManager {id} JSON missing owner"))?;
    if owner.to_lowercase() != wrap.signer.address.to_string().to_lowercase() {
        bail!("owned by {owner}, not our wallet {}", wrap.signer.address);
    }
    Ok(())
}

// -- Funding -------------------------------------------------------------

/// One funding sweep: for every configured target, top the mirrored escrow
/// back to target when it is below half. Mint failures alert but never kill
/// the loop.
async fn funding_pass(s: &Shared, snapshot: &token_info_client::Snapshot) {
    let balances = match s.ob.balances(&s.manager).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "funding: balance read failed");
            return;
        }
    };
    let by_type: HashMap<String, u64> = balances
        .iter()
        .filter_map(|b| {
            canonicalize_move_type(&b.token).ok().map(|t| (t, b.amount_raw()))
        })
        .collect();

    let tokens = match snapshot.test_tokens() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "funding: no test tokens in catalog");
            return;
        }
    };
    for (ticker, target) in &s.cfg.funding.targets {
        let token = match tokens.get(ticker) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(ticker, error = %format!("{e:#}"), "funding: unknown ticker");
                continue;
            }
        };
        let canonical = match canonicalize_move_type(&token.coin_type) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(ticker, error = %e, "funding: bad coin type");
                continue;
            }
        };
        let current = by_type.get(&canonical).copied().unwrap_or(0);
        metrics::gauge!("staging_mm_bot_escrow_raw", "ticker" => ticker.clone())
            .set(current as f64);
        if current >= target / 2 {
            continue;
        }
        let amount = target - current;
        let (pkg, module) = match token.module_path() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(ticker, error = %format!("{e:#}"), "funding: bad module path");
                continue;
            }
        };
        let faucet = match token.faucet() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(ticker, error = %format!("{e:#}"), "funding: bad faucet id");
                continue;
            }
        };
        match exchange_tx::mint_and_deposit_into_balance_manager(
            &s.wrap.client,
            &s.wrap.signer,
            pkg,
            &module,
            faucet,
            s.exchange_package,
            s.manager_oid,
            &token.coin_type,
            amount,
            s.gas_budget,
        )
        .await
        {
            Ok(_) => {
                tracing::info!(ticker, amount, "funding: minted and deposited");
                metrics::counter!("staging_mm_bot_mints_total", "ticker" => ticker.clone())
                    .increment(1);
            }
            Err(e) => {
                tracing::error!(
                    alert_id = "tx-failed-staging-mm-bot",
                    ticker,
                    amount,
                    error = %format!("{e:#}"),
                    "funding: mint+deposit failed"
                );
            }
        }
    }
}

// -- Quoting -------------------------------------------------------------

struct OpenOrder {
    digest: Digest,
    salt: u64,
    expiry_ms: u64,
}

async fn quote_loop(s: Arc<Shared>, ctx: MarketCtx) {
    let symbol = ctx.market.symbol.clone();
    let mut open: Vec<OpenOrder> = Vec::new();
    let mut last_mid: Option<u64> = None;
    let mut paused = false;
    let mut tick = tokio::time::interval(Duration::from_secs(s.cfg.quoting.refresh_secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;

        let spot = compute_spot_from_cache(
            &s.cache,
            ctx.base_feed,
            ctx.quote_feed,
            ctx.base_decimals,
            ctx.quote_decimals,
            s.staleness,
        );
        let mid = spot.ok().and_then(|sp| ladder::mid_ticks(sp, &ctx.market));
        let Some(mid) = mid else {
            // Stale/unusable price: pull the ladder and wait it out.
            if !paused {
                tracing::warn!(
                    market = %symbol,
                    reason = %spot.err().map(|e| e.as_str()).unwrap_or("mid off grid"),
                    "pausing quotes"
                );
                paused = true;
            }
            metrics::gauge!("staging_mm_bot_paused", "market" => symbol.clone()).set(1.0);
            if !open.is_empty() {
                pull_quotes(&s, &ctx, &mut open).await;
                last_mid = None;
            }
            continue;
        };
        if paused {
            tracing::info!(market = %symbol, "resuming quotes");
            paused = false;
        }
        metrics::gauge!("staging_mm_bot_paused", "market" => symbol.clone()).set(0.0);
        metrics::gauge!("staging_mm_bot_mid_ticks", "market" => symbol.clone()).set(mid as f64);

        let now = now_ms();
        let expiring = open
            .iter()
            .any(|o| o.expiry_ms.saturating_sub(now) < s.cfg.quoting.ttl_secs * 1_000 / 3);
        let drifted = match last_mid {
            Some(prev) if prev > 0 => {
                (mid.abs_diff(prev) as u128) * 10_000 >= (prev as u128) * s.cfg.quoting.requote_drift_bps as u128
            }
            _ => true,
        };
        if !open.is_empty() && !drifted && !expiring {
            continue;
        }

        match requote(&s, &ctx, &mut open, mid).await {
            Ok(placed) => {
                last_mid = Some(mid);
                metrics::gauge!("staging_mm_bot_open_orders", "market" => symbol.clone())
                    .set(placed as f64);
            }
            Err(e) => {
                tracing::warn!(market = %symbol, error = %format!("{e:#}"), "requote failed");
            }
        }
    }
}

/// Soft-cancel everything we have resting (frees the mirrored escrow
/// commitment before the new batch is intaken). Best-effort: the on-chain
/// watermark raise is what actually voids them — immediate here, because a
/// stale-price pull is the dead-man case the watermark exists for.
async fn pull_quotes(s: &Shared, ctx: &MarketCtx, open: &mut Vec<OpenOrder>) {
    let max_salt = open.iter().map(|o| o.salt).max();
    for o in open.drain(..) {
        let (sig, pk) = s.signer.sign_cancel(&o.digest);
        if let Err(e) = s.ob.cancel_order(&o.digest, &sig, &pk).await {
            tracing::warn!(error = %format!("{e:#}"), "soft cancel failed");
        }
    }
    if let Some(salt) = max_salt {
        raise_watermark(s, ctx, salt).await;
    }
}

/// Immediate single-market watermark raise — the reactive paths only
/// (stale-price pull, INSUFFICIENT_ESCROW recovery). Steady-state requotes
/// queue into the batched sweep instead.
async fn raise_watermark(s: &Shared, ctx: &MarketCtx, min_valid_salt: u64) {
    match exchange_tx::cancel_up_to(
        &s.wrap.client,
        &s.wrap.signer,
        s.exchange_package,
        ctx.registry,
        &ctx.market.base,
        &ctx.market.quote,
        min_valid_salt,
        s.gas_budget,
    )
    .await
    {
        Ok(_) => {
            let mut map = s.watermarks.lock().unwrap();
            let w = watermark_entry(&mut map, ctx);
            w.raised = w.raised.max(min_valid_salt);
            if w.pending.is_some_and(|p| p <= w.raised) {
                w.pending = None;
            }
        }
        Err(e) => {
            // A watermark that lags leaves soft-cancelled orders fillable
            // on-chain — alert, queue for the sweep to retry, keep quoting.
            tracing::error!(
                alert_id = "tx-failed-staging-mm-bot",
                market = %ctx.market.symbol,
                min_valid_salt,
                error = %format!("{e:#}"),
                "cancel_up_to failed"
            );
            record_pending_watermark(s, ctx, min_valid_salt);
        }
    }
}

fn watermark_entry<'a>(
    map: &'a mut HashMap<ObjectID, MarketWatermark>,
    ctx: &MarketCtx,
) -> &'a mut MarketWatermark {
    map.entry(ctx.registry).or_insert_with(|| MarketWatermark {
        base: ctx.market.base.clone(),
        quote: ctx.market.quote.clone(),
        pending: None,
        raised: 0,
    })
}

/// Queue a watermark raise for the batched sweep, keeping the per-market
/// max. Values at or below the known on-chain watermark are dropped.
fn record_pending_watermark(s: &Shared, ctx: &MarketCtx, min_valid_salt: u64) {
    let mut map = s.watermarks.lock().unwrap();
    let w = watermark_entry(&mut map, ctx);
    if min_valid_salt > w.raised {
        w.pending = Some(w.pending.map_or(min_valid_salt, |p| p.max(min_valid_salt)));
    }
}

/// One sweep pass: raise every pending watermark in a single PTB. Nothing
/// is cleared until the tx lands, so a failed sweep retries next tick; a
/// reactive raise that overtook a pending value filters it out here.
async fn watermark_sweep(s: &Shared) {
    let targets: Vec<exchange_tx::CancelUpToTarget> = {
        let map = s.watermarks.lock().unwrap();
        map.iter()
            .filter_map(|(registry, w)| {
                let salt = w.pending.filter(|p| *p > w.raised)?;
                Some(exchange_tx::CancelUpToTarget {
                    registry_id: *registry,
                    base_type: w.base.clone(),
                    quote_type: w.quote.clone(),
                    min_valid_salt: salt,
                })
            })
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    match exchange_tx::cancel_up_to_batch(
        &s.wrap.client,
        &s.wrap.signer,
        s.exchange_package,
        &targets,
        s.gas_budget,
    )
    .await
    {
        Ok(_) => {
            let mut map = s.watermarks.lock().unwrap();
            for t in &targets {
                if let Some(w) = map.get_mut(&t.registry_id) {
                    w.raised = w.raised.max(t.min_valid_salt);
                    if w.pending.is_some_and(|p| p <= w.raised) {
                        w.pending = None;
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!(
                alert_id = "tx-failed-staging-mm-bot",
                markets = targets.len(),
                error = %format!("{e:#}"),
                "batched cancel_up_to failed"
            );
        }
    }
}

/// Cancel-replace one market's ladder around `mid`. Returns how many orders
/// now rest.
async fn requote(
    s: &Shared,
    ctx: &MarketCtx,
    open: &mut Vec<OpenOrder>,
    mid: u64,
) -> Result<usize> {
    // Escrow budget per maker token, from the same mirror intake checks.
    let balances = s.ob.balances(&s.manager).await.context("reading escrow")?;
    let mut budget: HashMap<String, u64> = balances
        .iter()
        .filter_map(|b| {
            canonicalize_move_type(&b.token).ok().map(|t| {
                (t, (b.amount_raw() as f64 * s.cfg.quoting.escrow_utilization) as u64)
            })
        })
        .collect();

    // Free our own resting commitment first.
    if !open.is_empty() {
        for o in open.drain(..) {
            let (sig, pk) = s.signer.sign_cancel(&o.digest);
            if let Err(e) = s.ob.cancel_order(&o.digest, &sig, &pk).await {
                tracing::warn!(error = %format!("{e:#}"), "soft cancel failed");
            }
        }
    }

    let levels = ladder::build_ladder(mid, &s.cfg.quoting.levels);
    let expiry_ms = now_ms() + s.cfg.quoting.ttl_secs * 1_000;
    let mut batch_min_salt = None;
    for level in levels {
        let salt = s.salts.next();
        let Some(order) = ladder::make_order(
            &level,
            &ctx.market,
            s.signer.address(),
            s.manager,
            s.cfg.max_fee_bps,
            expiry_ms,
            salt,
        ) else {
            continue;
        };
        // Budget: skip levels the escrow can't cover.
        let avail = budget.entry(order.maker_token.clone()).or_insert(0);
        if *avail < order.maker_amount {
            tracing::debug!(
                market = %ctx.market.symbol,
                ?level,
                "skipping level over escrow budget"
            );
            continue;
        }
        *avail -= order.maker_amount;

        let (digest, signed) = s.signer.sign_order(order, ctx.market.registry_id);
        match s.ob.place_order(&signed).await? {
            Ok(resp) => {
                batch_min_salt.get_or_insert(salt);
                metrics::counter!("staging_mm_bot_orders_placed_total", "market" => ctx.market.symbol.clone())
                    .increment(1);
                if resp.status != "SELF_TRADE_CANCELLED" {
                    open.push(OpenOrder { digest, salt, expiry_ms });
                }
            }
            Err(IntakeReject { code, detail }) => {
                metrics::counter!("staging_mm_bot_orders_rejected_total", "code" => code.clone())
                    .increment(1);
                if code == "INSUFFICIENT_ESCROW" {
                    // Stale (e.g. pre-restart) orders still hold commitment.
                    // If nothing of this batch rests yet, void everything
                    // below the current salt and retry next tick with fresh
                    // salts. Once part of the batch is live, just stop — a
                    // watermark at `salt` would void the orders we placed
                    // moments ago.
                    tracing::warn!(market = %ctx.market.symbol, detail, "escrow busy; stopping batch");
                    if batch_min_salt.is_none() {
                        raise_watermark(s, ctx, salt).await;
                    }
                    break;
                }
                // Anything else is our bug (OFF_TICK, EXPIRY, SALT…): loud.
                tracing::error!(market = %ctx.market.symbol, code, detail, "order rejected");
            }
        }
    }

    // Queue the void of every prior batch for the batched sweep; the new
    // batch's salts sit above the pending watermark and stay live. Deferring
    // is safe because replaced orders expire on-chain within ttl_secs.
    if let Some(min_salt) = batch_min_salt {
        record_pending_watermark(s, ctx, min_salt.saturating_sub(1));
    }
    Ok(open.len())
}
