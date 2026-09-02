//! staging-mm-bot — faucet-funded maker bot for the hybrid exchange (SO-368).
//!
//! Boot: config + secrets (hard-gated to testnet), token-info catalog,
//! orderbook market discovery, oracle-service WS price cache, then a
//! BalanceManager (adopt-or-create) funded from the test-token faucets —
//! or, in vault-direct mode, a trading vault resolved by the `vault`
//! module (adopted or self-provisioned + wired), so a contract redeploy
//! needs no manual re-pin.
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
use move_core_types::identifier::Identifier;
use serde::Deserialize;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;

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

    /// Vault-direct mode (SO-372): quote as a trading vault's delegated
    /// signer against its direct-custody identity BM. Absent ⇒ the classic
    /// funded-BM mode (create/adopt + faucet funding) runs unchanged.
    #[serde(default)]
    vault_direct: Option<VaultDirectConfig>,
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
        if let Some(vd) = &self.vault_direct {
            if vd.buffer_bps >= 10_000 {
                bail!("[vault_direct].buffer_bps must be < 10000");
            }
        }
        Ok(())
    }
}

/// Vault-direct mode config (SO-372). The vault and its identity BM are
/// resolved at boot — adopted from chain/indexer state, or provisioned and
/// wired by the bot itself (see [`staging_mm_bot::vault`]) — so nothing
/// here needs re-pinning after a contract redeploy.
#[derive(Debug, Clone, Deserialize)]
struct VaultDirectConfig {
    /// Optional pin: adopt this vault whatever its provenance (the
    /// operator's escape hatch for a human-provisioned vault). Verified
    /// against chain state, never trusted. Empty ⇒ adopt a self-created
    /// vault or provision one.
    #[serde(default)]
    vault_id: String,
    /// api-service base URL for the pending-withdrawals deduction
    /// (`GET /trading-vaults/:id/pending-requests`). Absent ⇒ skip it.
    #[serde(default)]
    api_url: Option<String>,
    /// Fraction of the vault's free balance held back from quoting, bps.
    #[serde(default = "default_buffer_bps")]
    buffer_bps: u64,
    /// SO-418: escrowed curator-commitment funding (`deposit_into_commitment`),
    /// accounting-asset raw units. The funding pass funds it when the
    /// slot is missing (this wallet holds the CuratorCap of a vault it
    /// provisioned). 0 disables — but an unfunded commitment trips the
    /// `curator_commitment_breached` gate once the protocol floor is
    /// nonzero, which parks the bot's quoting. Default $100k at 6 decimals.
    #[serde(default = "default_commitment_deposit")]
    commitment_deposit: u64,
    /// Self-provisioning when discovery finds no vault of ours.
    #[serde(default)]
    provision: staging_mm_bot::vault::ProvisionConfig,
}

fn default_buffer_bps() -> u64 {
    1_000
}

fn default_commitment_deposit() -> u64 {
    100_000_000_000
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

/// Vault-direct runtime context (SO-372), resolved at boot.
struct DirectCtx {
    vault: ObjectID,
    trading_vault_pkg: ObjectID,
    api_url: Option<String>,
    buffer_bps: u64,
    /// The vault's CuratorCap when this wallet holds it (SO-418) —
    /// commitment funding needs it; `None` disables commitment upkeep.
    curator_cap: Option<ObjectID>,
    /// `[vault_direct].commitment_deposit`, accounting raw units.
    commitment_deposit: u64,
    /// Tranche wire code the funding pass deposits into (SO-420): 0 on an
    /// untranched vault, junior on a tranched one.
    deposit_tranche: u8,
    /// Indexer view for the SO-418 risk gate + tranche budget measure.
    indexer: indexer_graphql::IndexerClient,
    /// Latch so the risk-off hard-stop logs on transition, not per tick.
    risk_off_latched: std::sync::atomic::AtomicBool,
    /// Funding refs (SO-375); `None` when `[funding]` is disabled or has
    /// no targets.
    funding: Option<DirectFundingRefs>,
}

/// Everything the vault-direct funding pass needs to compose an
/// attestation-bearing deposit (SO-375), resolved once at boot from
/// token-info's `tradingVaultObjects` block.
struct DirectFundingRefs {
    protocol_config_id: ObjectID,
    /// Shared `whitelist::Whitelist` — `vault::deposit`'s ingress gate
    /// (SO-383).
    whitelist_id: ObjectID,
    oracle_registry_id: ObjectID,
    deepbook_adapter_pkg: Option<ObjectID>,
    options_adapter_pkg: Option<ObjectID>,
    exchange_adapter_pkg: Option<ObjectID>,
    equity_oracle_pkg: Option<ObjectID>,
    equity_book_id: Option<ObjectID>,
    vol_book_id: Option<ObjectID>,
    /// oracle-service REST client for `/oracle/descriptor` + `/oracle/legs`
    /// (the WS price cache is separate and unaffected).
    oracle: oracle_client::OracleClient,
}

/// Shared handles across tasks.
struct Shared {
    wrap: SuiClientWrapper,
    ob: OrderbookClient,
    signer: OrderSigner,
    /// The order's `maker` field: the wallet in funded mode, the vault's
    /// id-as-address in vault-direct mode (the identity BM's owner).
    maker: ExAddress,
    manager: ExAddress,
    manager_oid: ObjectID,
    exchange_package: ObjectID,
    /// Shared ingress `Whitelist` (standalone whitelist package, SO-384).
    /// `None` on records predating it — BM deposits then fail loudly.
    whitelist: Option<ObjectID>,
    salts: SaltSource,
    cfg: BotConfig,
    gas_budget: u64,
    cache: PriceCache,
    staleness: Staleness,
    watermarks: Mutex<HashMap<ObjectID, MarketWatermark>>,
    /// Funding-pass backoff per ticker / commitment (SO-461).
    funding_backoff: Mutex<staging_mm_bot::funding_backoff::Backoff>,
    /// `Some` in vault-direct mode.
    direct: Option<DirectCtx>,
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
    // Ingress whitelist (SO-384) for BM deposits, from the standalone
    // whitelist block of the record token-info serves.
    let whitelist = snapshot.whitelist_object()?;

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

    // BalanceManager. Vault-direct mode (SO-372): resolve the vault —
    // adopt a pinned or self-created one, or provision and wire a fresh
    // vault (custody + adapter + signer delegation + deposit allowlist) —
    // so a contract redeploy needs no manual re-pin. Funded mode:
    // rediscover the BM this wallet already created under the CURRENT
    // exchange package, else create one. Zero config either way: a
    // redeploy changes the packages, discovery scopes itself to the live
    // deployment, and fresh objects appear on the first boot after it.
    let (manager_oid, maker, direct) = match &cfg.vault_direct {
        Some(vd) => {
            let trading_vault_pkg = snapshot
                .trading_vault()
                .ok_or_else(|| anyhow!("vault-direct mode but token-info has no tradingVault package"))?
                .package()?;
            let exchange_adapter_pkg = snapshot
                .exchange_adapter()
                .ok_or_else(|| {
                    anyhow!("vault-direct mode but token-info has no exchangeAdapter package")
                })?
                .package()?;
            let tvo_boot = snapshot.trading_vault_objects().ok_or_else(|| {
                anyhow!("vault-direct mode but token-info has no tradingVaultObjects block")
            })?;
            let whitelist_id = whitelist.ok_or_else(|| {
                anyhow!(
                    "vault-direct mode but token-info has no whitelist block — vault \
                     creation and deposits are ingress-gated (SO-383)"
                )
            })?;
            // The vault's accounting asset is the one quote asset every
            // market shares; the funding targets are what must be
            // depositable into it.
            let accounting = staging_mm_bot::vault::unique_accounting(
                &market_ctxs.iter().map(|c| c.market.quote.clone()).collect::<Vec<_>>(),
            )?;
            let deposit_types: Vec<String> = match snapshot.test_tokens() {
                Ok(tokens) => cfg
                    .funding
                    .targets
                    .keys()
                    .filter_map(|ticker| match tokens.get(ticker) {
                        Ok(t) => canonicalize_move_type(&t.coin_type).ok(),
                        Err(e) => {
                            tracing::warn!(ticker, error = %format!("{e:#}"), "unknown funding ticker");
                            None
                        }
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            let indexer =
                indexer_graphql::IndexerClient::new(cli.indexer_graphql_url.clone());
            let resolved = staging_mm_bot::vault::resolve(staging_mm_bot::vault::ResolveParams {
                wrap: &wrap,
                indexer: &indexer,
                cfg: &vd.provision,
                pinned_vault_id: vd.vault_id.trim(),
                trading_vault_package: trading_vault_pkg,
                exchange_adapter_package: exchange_adapter_pkg,
                exchange_package,
                integration_registry: tvo_boot.integration_registry()?,
                vault_protocol_config: tvo_boot.vault_protocol_config()?,
                whitelist: whitelist_id,
                accounting_coin_type: &accounting,
                deposit_asset_types: &deposit_types,
            })
            .await
            .inspect_err(staging_mm_bot::vault::report_unusable)?;
            let manager_oid = resolved.manager_id;
            let vault = resolved.vault_id;
            let maker = ExAddress::parse(&vault.to_string())
                .map_err(|e| anyhow!("vault id hex: {e}"))?;
            tracing::info!(
                manager = %manager_oid,
                vault = %vault,
                provisioned = resolved.provisioned,
                "vault-direct mode: vault resolved; fills escrow against vault free balances"
            );
            // Funding refs (SO-375): the bot LPs into the vault with real
            // attested deposits, so it needs the appraisal-composer ids.
            let funding = if cfg.funding.enabled && !cfg.funding.targets.is_empty() {
                let tvo = snapshot.trading_vault_objects().ok_or_else(|| {
                    anyhow!(
                        "[funding] enabled in vault-direct mode but token-info has no \
                         tradingVaultObjects block"
                    )
                })?;
                Some(DirectFundingRefs {
                    protocol_config_id: tvo.vault_protocol_config()?,
                    whitelist_id: snapshot.whitelist_object()?.ok_or_else(|| {
                        anyhow!(
                            "[funding] enabled but token-info has no whitelist block — \
                             vault deposits are ingress-gated (SO-383)"
                        )
                    })?,
                    oracle_registry_id: tvo.oracle_registry()?,
                    deepbook_adapter_pkg: snapshot
                        .deepbook_adapter()
                        .map(|p| p.package())
                        .transpose()?,
                    options_adapter_pkg: snapshot
                        .options_adapter()
                        .map(|p| p.package())
                        .transpose()?,
                    exchange_adapter_pkg: snapshot
                        .exchange_adapter()
                        .map(|p| p.package())
                        .transpose()?,
                    equity_oracle_pkg: snapshot
                        .equity_oracle()
                        .map(|p| p.package())
                        .transpose()?,
                    equity_book_id: tvo
                        .equity_book_id
                        .as_deref()
                        .map(ObjectID::from_str)
                        .transpose()
                        .context("parsing equityBookId")?,
                    vol_book_id: tvo
                        .vol_book_id
                        .as_deref()
                        .map(ObjectID::from_str)
                        .transpose()
                        .context("parsing volBookId")?,
                    oracle,
                })
            } else {
                None
            };
            let direct = DirectCtx {
                vault,
                trading_vault_pkg,
                api_url: vd.api_url.clone(),
                buffer_bps: vd.buffer_bps,
                curator_cap: resolved.curator_cap,
                commitment_deposit: vd.commitment_deposit,
                deposit_tranche: resolved.deposit_tranche,
                indexer,
                risk_off_latched: std::sync::atomic::AtomicBool::new(false),
                funding,
            };
            (manager_oid, maker, Some(direct))
        }
        None => {
            let manager_oid =
                resolve_balance_manager(&wrap, exchange_package, cli.gas_budget).await?;
            tracing::info!(manager = %manager_oid, "BalanceManager ready");
            (manager_oid, signer.address(), None)
        }
    };
    let manager = ExAddress::parse(&manager_oid.to_string())
        .map_err(|e| anyhow!("manager id hex: {e}"))?;

    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
        max_conf_bps: cfg.pyth.max_conf_bps,
    };
    let shared = Arc::new(Shared {
        wrap,
        ob,
        signer,
        maker,
        manager,
        manager_oid,
        exchange_package,
        whitelist,
        salts: SaltSource::new(),
        cfg,
        gas_budget: cli.gas_budget,
        cache,
        staleness,
        watermarks: Mutex::new(HashMap::new()),
        funding_backoff: Mutex::new(Default::default()),
        direct,
    });

    // Seed escrow before quoting so the first ladder doesn't bounce off
    // INSUFFICIENT_ESCROW, then keep it topped up in the background.
    // Vault-direct mode (SO-375) funds by DEPOSITING into the vault —
    // faucet mint + attested `vault::deposit`, shares to this wallet, pps
    // untouched — the infinite-money LP, never a NAV donation.
    if let Some(d) = &shared.direct {
        if d.funding.is_some() {
            direct_funding_pass(&shared, &snapshot).await;
            let s = Arc::clone(&shared);
            let snap = snapshot.clone();
            tokio::spawn(async move {
                let mut tick =
                    tokio::time::interval(Duration::from_secs(s.cfg.funding.check_interval_secs));
                tick.tick().await; // immediate first tick already ran above
                loop {
                    tick.tick().await;
                    direct_funding_pass(&s, &snap).await;
                }
            });
        } else {
            tracing::info!("vault-direct mode: funding disabled by config");
        }
    } else if shared.cfg.funding.enabled {
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
    let owner = staging_mm_bot::vault::manager_owner(&wrap.client, id).await?;
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
    // balance_manager::deposit is whitelist-gated (SO-384); without the
    // object id there is nothing this pass can do.
    let Some(whitelist) = s.whitelist else {
        tracing::error!(
            "funding: deployments record has no whitelist block — BM deposits are \
             ingress-gated (SO-384); redeploy the protocol"
        );
        return;
    };
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
            whitelist,
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

/// Vault-direct funding sweep (SO-375): the bot is an LP with an infinite
/// pocket. For every configured target whose vault free balance fell below
/// half, faucet-mint the shortfall and DEPOSIT it — a real share-minting
/// `vault::deposit` valued by a fresh price attestation, so pps stays
/// honest (never a balance donation). Shares accrue to this wallet and
/// are never withdrawn. Failures alert but never kill the loop.
async fn direct_funding_pass(s: &Shared, snapshot: &token_info_client::Snapshot) {
    let Some(d) = &s.direct else { return };
    let Some(f) = &d.funding else { return };
    let tokens = match snapshot.test_tokens() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "funding: no test tokens in catalog");
            return;
        }
    };

    // SO-418: does the escrowed curator commitment exist yet? Funded here
    // (not at provision) because this pass owns the appraisal composer —
    // by now the vault may hold several appraised assets.
    let need_commitment = match d.curator_cap {
        Some(cap) if d.commitment_deposit > 0 => {
            match staging_mm_bot::vault::has_commitment(
                &s.wrap.client,
                s.wrap.signer.address,
                d.trading_vault_pkg,
                d.vault,
                cap,
            )
            .await
            {
                Ok(exists) => !exists,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "funding: commitment presence read failed");
                    false
                }
            }
        }
        _ => false,
    };

    // Cheap balance reads first; the oracle round-trips only run when
    // something actually needs topping up.
    let mut shortfalls: Vec<(String, &token_info_client::TokenInfo, String, u64)> = Vec::new();
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
        let free = match sui_tx::tx::trading_vault::dev_inspect_free_balance(
            &s.wrap.client,
            s.wrap.signer.address,
            d.trading_vault_pkg,
            d.vault,
            &canonical,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(ticker, error = %format!("{e:#}"), "funding: free-balance read failed");
                continue;
            }
        };
        metrics::gauge!("staging_mm_bot_escrow_raw", "ticker" => ticker.clone())
            .set(free as f64);
        if free >= target / 2 {
            continue;
        }
        shortfalls.push((ticker.clone(), token, canonical, target - free));
    }
    if shortfalls.is_empty() && !need_commitment {
        return;
    }

    let descriptor = match f.oracle.descriptor().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "funding: /oracle/descriptor unreachable");
            return;
        }
    };
    if descriptor.provider != protocol_types::OracleProvider::Switchboard {
        tracing::error!(
            provider = %descriptor.provider,
            "funding: live oracle provider is not Switchboard — vault-direct funding \
             deposits are Switchboard-only; skipping this pass"
        );
        return;
    }

    // Commitment first (SO-418): funding it also cures a commitment
    // breach, which un-parks quoting.
    if need_commitment {
        let cap = d.curator_cap.expect("need_commitment implies a cap");
        let now = Instant::now();
        let blocked = s.funding_backoff.lock().unwrap().blocked("commitment", now);
        if let Some(left) = blocked {
            tracing::warn!(retry_in_secs = left.as_secs(), "funding: commitment deposit backing off after failures");
        } else {
        match fund_commitment(s, d, f, tokens, &descriptor, cap).await {
            Ok(_) => {
                s.funding_backoff.lock().unwrap().succeed("commitment");
                tracing::info!(
                    amount = d.commitment_deposit,
                    "funding: escrowed curator commitment funded"
                );
            }
            Err(e) => {
                tracing::error!(
                    alert_id = "tx-failed-staging-mm-bot",
                    amount = d.commitment_deposit,
                    error = %format!("{e:#}"),
                    "funding: commitment deposit failed"
                );
                let n = s.funding_backoff.lock().unwrap().fail("commitment", now);
                tracing::warn!(streak = n, next_delay_secs = staging_mm_bot::funding_backoff::delay(n).as_secs(), "funding: commitment deposit backing off");
            }
        }
        }
    }

    let mut deposited = false;
    for (ticker, token, canonical, amount) in shortfalls {
        // Holdings per deposit, not per pass: each landed deposit changes
        // the free-balance set, and the next appraisal must cover it.
        let holdings =
            match sui_tx::tx::appraisal::discover_holdings(&s.wrap.client, d.vault).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "funding: holdings discovery failed");
                    return;
                }
            };
        let now = Instant::now();
        let blocked = s.funding_backoff.lock().unwrap().blocked(&ticker, now);
        if let Some(left) = blocked {
            tracing::debug!(ticker, retry_in_secs = left.as_secs(), "funding: deposit backing off after failures");
            continue;
        }
        match direct_deposit(s, d, f, &holdings, &descriptor, token, &canonical, amount).await {
            Ok(_) => {
                s.funding_backoff.lock().unwrap().succeed(&ticker);
                deposited = true;
                tracing::info!(ticker, amount, "funding: minted and deposited into vault");
                metrics::counter!("staging_mm_bot_mints_total", "ticker" => ticker.clone())
                    .increment(1);
            }
            Err(e) => {
                let n = s.funding_backoff.lock().unwrap().fail(&ticker, now);
                let delay = staging_mm_bot::funding_backoff::delay(n).as_secs();
                // Alert on the FIRST failure of a streak; the rest are the
                // same fault backing off, not new incidents.
                if n == 1 {
                    tracing::error!(
                        alert_id = "tx-failed-staging-mm-bot",
                        ticker,
                        amount,
                        error = %format!("{e:#}"),
                        next_delay_secs = delay,
                        "funding: vault deposit failed"
                    );
                } else {
                    tracing::warn!(ticker, amount, streak = n, next_delay_secs = delay, error = %format!("{e:#}"), "funding: vault deposit still failing; backing off");
                }
            }
        }
    }

    // SO-418 custody: every deposit above minted a `VaultPosition` NFT
    // into this wallet (discovered by owned-object query — transfers emit
    // no events). Merge to one per tranche × generation so the object
    // count stays bounded across an infinite-pocket LP's lifetime.
    if deposited {
        match staging_mm_bot::positions::merge_owned_positions(
            &s.wrap,
            d.trading_vault_pkg,
            d.vault,
            s.gas_budget,
        )
        .await
        {
            Ok(folded) if folded > 0 => {
                tracing::info!(folded, "funding: merged wallet VaultPositions")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "funding: position merge failed; next pass retries")
            }
        }
    }
}

/// SO-418: fund the escrowed curator commitment — full composed appraisal
/// (the vault may hold several appraised assets by now), faucet-mint of
/// the ACCOUNTING asset, `vault::deposit_into_commitment`, one PTB.
async fn fund_commitment(
    s: &Shared,
    d: &DirectCtx,
    f: &DirectFundingRefs,
    tokens: &token_info_client::TestTokens,
    descriptor: &oracle_client::OracleDescriptor,
    curator_cap: ObjectID,
) -> Result<()> {
    let holdings = sui_tx::tx::appraisal::discover_holdings(&s.wrap.client, d.vault)
        .await
        .context("holdings discovery for the commitment deposit")?;
    let accounting = holdings.deposit_type.clone();
    let token = tokens
        .tokens
        .values()
        .find(|t| {
            canonicalize_move_type(&t.coin_type).is_ok_and(|c| c == accounting)
        })
        .ok_or_else(|| anyhow!("no faucet for the accounting asset {accounting}"))?;

    let refs = sui_tx::tx::appraisal::AppraisalRefs {
        trading_vault_pkg: d.trading_vault_pkg,
        deepbook_adapter_pkg: f.deepbook_adapter_pkg,
        options_adapter_pkg: f.options_adapter_pkg,
        exchange_adapter_pkg: f.exchange_adapter_pkg,
        vault_id: d.vault,
        protocol_config_id: f.protocol_config_id,
        oracle_registry_id: f.oracle_registry_id,
        equity_oracle_pkg: f.equity_oracle_pkg,
        equity_book_id: f.equity_book_id,
        vol_book_id: f.vol_book_id,
    };
    let mut pt = ProgrammableTransactionBuilder::new();
    let (appraisal, _attestations) = sui_tx::tx::appraisal::compose_switchboard_appraisal(
        &s.wrap.client,
        &mut pt,
        &refs,
        &holdings,
        &std::collections::BTreeMap::new(),
        descriptor,
        &f.oracle,
        &[],
    )
    .await
    .context("composing appraisal for the commitment deposit")?;

    let (tokens_pkg, module) = token.module_path()?;
    let faucet =
        pt.obj(sui_tx::tx::shared_object_arg(&s.wrap.client, token.faucet()?, true).await?)?;
    let amount_arg = pt.pure(d.commitment_deposit)?;
    let coin = pt.programmable_move_call(
        tokens_pkg,
        Identifier::new(&*module).map_err(|e| anyhow!("module name {module}: {e}"))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount_arg],
    );

    let tv_refs = sui_tx::tx::trading_vault::TradingVaultRefs {
        package: d.trading_vault_pkg,
        vault_id: d.vault,
        protocol_config_id: f.protocol_config_id,
        deposit_type: &accounting,
    };
    sui_tx::tx::trading_vault::build_deposit_into_commitment(
        &s.wrap.client,
        &mut pt,
        &tv_refs,
        f.whitelist_id,
        curator_cap,
        appraisal,
        coin,
    )
    .await?;
    simulate_then_submit(s, pt, "commitment deposit").await?;
    Ok(())
}

/// Dev-inspect first — it is free — and only pay gas for a transaction the
/// simulation says will land (SO-461). A Move abort in the simulation is
/// returned as an error without touching the chain.
async fn simulate_then_submit(
    s: &Shared,
    pt: ProgrammableTransactionBuilder,
    label: &str,
) -> Result<()> {
    use sui_types::effects::TransactionEffectsAPI;
    let programmable = pt.finish();
    let inspect_tx = sui_types::transaction::TransactionData::new_programmable(
        s.wrap.signer.address,
        vec![],
        programmable.clone(),
        50_000_000_000,
        1_000,
    );
    let inspect = s
        .wrap
        .client
        .dev_inspect(&inspect_tx)
        .await
        .with_context(|| format!("{label}: pre-simulation"))?;
    let status = inspect.transaction.effects.status();
    if status.is_err() {
        bail!("{label}: pre-simulation aborted, not submitted (no gas spent): {status:?}");
    }
    sui_tx::tx::submit_ptb_rebuilding(&s.wrap.client, &s.wrap.signer, s.gas_budget, label, || {
        let p = programmable.clone();
        async move { Ok(p) }
    })
    .await?;
    Ok(())
}

/// One attested vault deposit (SO-375): compose the full appraisal (all
/// held assets get legs; the deposited asset rides as `extra_attest` when
/// it isn't the accounting asset), faucet-mint the shortfall, and
/// `vault::deposit<T>` it — all in one PTB.
#[allow(clippy::too_many_arguments)]
async fn direct_deposit(
    s: &Shared,
    d: &DirectCtx,
    f: &DirectFundingRefs,
    holdings: &sui_tx::tx::appraisal::VaultHoldings,
    descriptor: &oracle_client::OracleDescriptor,
    token: &token_info_client::TokenInfo,
    canonical: &str,
    amount: u64,
) -> Result<()> {
    let refs = sui_tx::tx::appraisal::AppraisalRefs {
        trading_vault_pkg: d.trading_vault_pkg,
        deepbook_adapter_pkg: f.deepbook_adapter_pkg,
        options_adapter_pkg: f.options_adapter_pkg,
        exchange_adapter_pkg: f.exchange_adapter_pkg,
        vault_id: d.vault,
        protocol_config_id: f.protocol_config_id,
        oracle_registry_id: f.oracle_registry_id,
        equity_oracle_pkg: f.equity_oracle_pkg,
        equity_book_id: f.equity_book_id,
        vol_book_id: f.vol_book_id,
    };
    let accounting = holdings.deposit_type.as_str();
    let is_accounting = canonical == accounting;
    let extras: Vec<String> =
        if is_accounting { Vec::new() } else { vec![canonical.to_string()] };

    let mut pt = ProgrammableTransactionBuilder::new();
    let (appraisal, attestations) = sui_tx::tx::appraisal::compose_switchboard_appraisal(
        &s.wrap.client,
        &mut pt,
        &refs,
        holdings,
        &std::collections::BTreeMap::new(),
        descriptor,
        &f.oracle,
        &extras,
    )
    .await
    .context("composing appraisal")?;

    // Faucet mint → Coin<T>, deposited in the same PTB.
    let (tokens_pkg, module) = token.module_path()?;
    let faucet = pt.obj(sui_tx::tx::shared_object_arg(&s.wrap.client, token.faucet()?, true).await?)?;
    let amount_arg = pt.pure(amount)?;
    let coin = pt.programmable_move_call(
        tokens_pkg,
        Identifier::new(&*module).map_err(|e| anyhow!("module name {module}: {e}"))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, amount_arg],
    );

    let tv_refs = sui_tx::tx::trading_vault::TradingVaultRefs {
        package: d.trading_vault_pkg,
        vault_id: d.vault,
        protocol_config_id: f.protocol_config_id,
        deposit_type: accounting,
    };
    // v2 (SO-418): deposits mint a `VaultPosition` NFT that MUST be
    // consumed — transfer it to this wallet. SO-420: the tranche code was
    // fixed at vault resolution (0 untranched, junior on a tranched
    // vault). The funding pass merges the accumulated positions
    // afterwards.
    if is_accounting {
        sui_tx::tx::trading_vault::build_deposit_and_transfer(
            &s.wrap.client,
            &mut pt,
            &tv_refs,
            f.whitelist_id,
            appraisal,
            coin,
            d.deposit_tranche,
            s.wrap.signer.address,
        )
        .await?;
    } else {
        let att = *attestations.get(canonical).ok_or_else(|| {
            anyhow!("no attestation composed for {canonical} — descriptor feed missing?")
        })?;
        let position = sui_tx::tx::trading_vault::build_deposit_asset(
            &s.wrap.client,
            &mut pt,
            &tv_refs,
            f.whitelist_id,
            canonical,
            appraisal,
            coin,
            att,
            d.deposit_tranche,
        )
        .await?;
        pt.transfer_arg(s.wrap.signer.address, position);
    }
    simulate_then_submit(s, pt, "vault deposit").await?;
    Ok(())
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
    // Vault-direct mode: the maker (watermark key) is the vault's address,
    // which can never be a tx sender — the manager-keyed target routes
    // through `cancel_up_to_for_manager`, raising the OWNER's watermark
    // instead of the bot wallet's.
    let target = exchange_tx::CancelUpToTarget {
        registry_id: ctx.registry,
        base_type: ctx.market.base.clone(),
        quote_type: ctx.market.quote.clone(),
        min_valid_salt,
        manager_id: s.direct.as_ref().map(|_| s.manager_oid),
    };
    match exchange_tx::cancel_up_to_batch(
        &s.wrap.client,
        &s.wrap.signer,
        s.exchange_package,
        std::slice::from_ref(&target),
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
/// reactive raise that overtook a pending value filters it out here. In
/// vault-direct mode every target is manager-keyed (SO-372).
async fn watermark_sweep(s: &Shared) {
    let manager_id = s.direct.as_ref().map(|_| s.manager_oid);
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
                    manager_id,
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

/// Per-token quote budget. Funded mode: the orderbook-mirrored escrow,
/// scaled by `escrow_utilization` (the same mirror intake checks). Direct
/// mode (SO-372): the vault's live free balance minus the configured
/// buffer, minus outstanding withdrawal-queue obligations when api-service
/// is configured. Markets share the vault's balance, so the buffer also
/// absorbs cross-market overcommit (funded mode has the same sharing).
///
/// SO-418: hard-stops (empty budget) while the vault is risk-off — fills
/// settle out of vault free balances, which the on-chain gate set blocks
/// (abort 124) — and on tranched vaults additionally caps the QUOTE-asset
/// budget by the junior/risk-bearing measure (the senior claim is not the
/// bot's to quote against; base-token floats are exchange inventory and
/// keep the free-balance bound).
async fn quote_budget(s: &Shared, ctx: &MarketCtx) -> Result<HashMap<String, u64>> {
    let Some(d) = &s.direct else {
        let balances = s.ob.balances(&s.manager).await.context("reading escrow")?;
        return Ok(balances
            .iter()
            .filter_map(|b| {
                canonicalize_move_type(&b.token).ok().map(|t| {
                    (t, (b.amount_raw() as f64 * s.cfg.quoting.escrow_utilization) as u64)
                })
            })
            .collect());
    };

    // SO-418 risk gate + tranche measure from the indexer view. An
    // unreachable indexer never blocks quoting (free balance is still the
    // on-chain truth); a MISSING vault row does not either (fresh vault,
    // indexer catching up).
    let mut junior_cap: Option<u64> = None;
    match vault_view(&d.indexer, d.vault).await {
        Ok(Some(v)) => {
            let off = staging_mm_bot::vault::risk_off(&v);
            let was = d.risk_off_latched.swap(off, Ordering::Relaxed);
            if off && !was {
                tracing::warn!(
                    vault = %d.vault,
                    risk_state = v.risk_state,
                    commitment_breached = v.curator_commitment_breached,
                    state = %v.state,
                    settled = v.settled,
                    "vault is RISK-OFF — quoting hard-stopped until it cures"
                );
            } else if !off && was {
                tracing::info!(vault = %d.vault, "vault risk state cured — quoting resumes");
            }
            metrics::gauge!("staging_mm_bot_vault_risk_off").set(if off { 1.0 } else { 0.0 });
            if off {
                return Ok(HashMap::new());
            }
            junior_cap = staging_mm_bot::vault::junior_capital(&v);
        }
        Ok(None) => tracing::debug!(vault = %d.vault, "vault not in indexer view; no risk gate"),
        Err(e) => tracing::debug!(error = %format!("{e:#}"), "risk-gate indexer read failed"),
    }

    let mut budget = HashMap::new();
    for token in [&ctx.market.base, &ctx.market.quote] {
        let free = sui_tx::tx::trading_vault::dev_inspect_free_balance(
            &s.wrap.client,
            s.wrap.signer.address,
            d.trading_vault_pkg,
            d.vault,
            token,
        )
        .await
        .with_context(|| format!("reading vault free balance of {token}"))?;
        let held_back = ((free as u128) * (d.buffer_bps as u128) / 10_000) as u64;
        let mut avail = free.saturating_sub(held_back);
        // Tranched vault: the quote asset (= accounting asset) budget is
        // additionally bounded by the junior/risk capital.
        if token == &ctx.market.quote {
            if let Some(cap) = junior_cap {
                avail = avail.min(cap);
            }
        }
        budget.insert(token.clone(), avail);
    }
    if let Some(api) = &d.api_url {
        match pending_obligations(api, &d.vault).await {
            Ok(pending) => {
                for (token, amount) in pending {
                    if let Some(b) = budget.get_mut(&token) {
                        *b = b.saturating_sub(amount);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "pending-requests read failed; quoting without the deduction"
                );
            }
        }
    }
    Ok(budget)
}

/// The bot's own vault row from the indexer view (SO-418 risk gate).
async fn vault_view(
    indexer: &indexer_graphql::IndexerClient,
    vault: ObjectID,
) -> Result<Option<indexer_graphql::TradingVault>> {
    let hex = vault.to_hex_literal();
    Ok(indexer
        .trading_vaults()
        .await
        .context("listing trading vaults")?
        .into_iter()
        .find(|v| format!("0x{}", v.vault_id.to_hex()) == hex))
}

/// One pending withdraw request as `/trading-vaults/:id/pending-requests`
/// serves it (SO-418 v2 shape). MIXED CASING on the wire, per the WS-3
/// DTO contract: the pre-v2 fields keep camelCase (`basisRaw`,
/// `payoutCoinType`, …) while every v2 addition is pinned snake_case
/// (`global_seq`, `lane_code`, `position_id`, `payable`,
/// `blocked_reason`, …) — hence the struct-level camelCase with explicit
/// renames, mirroring the server DTO.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingReq {
    basis_raw: String,
    payout_coin_type: String,
    /// v2: whether fulfillment could pay this request right now. A
    /// blocked (junior-lane / wiped) request is still deducted — it
    /// resumes at its own head when the lane unblocks, so the obligation
    /// stands either way. Read here so the shape stays pinned by test.
    #[serde(rename = "payable", default)]
    #[allow(dead_code)]
    payable: bool,
    #[serde(rename = "blocked_reason", default)]
    #[allow(dead_code)]
    blocked_reason: Option<String>,
}

#[derive(Deserialize)]
struct PendingResp {
    requests: Vec<PendingReq>,
}

/// Sum pending requests into per-payout-token obligations, estimated at
/// `basis` (accounting-asset units) — exact payout units are only fixed
/// at fulfillment, so this is the cheap conservative stand-in. Blocked
/// lanes still count (see [`PendingReq::payable`]).
fn sum_obligations(requests: Vec<PendingReq>) -> HashMap<String, u64> {
    let mut out: HashMap<String, u64> = HashMap::new();
    for r in requests {
        let token =
            canonicalize_move_type(&r.payout_coin_type).unwrap_or(r.payout_coin_type);
        let amount = r.basis_raw.parse::<u64>().unwrap_or(0);
        let entry = out.entry(token).or_insert(0);
        *entry = entry.saturating_add(amount);
    }
    out
}

/// Outstanding withdrawal-queue obligations by payout coin type, from
/// api-service `GET /trading-vaults/:id/pending-requests`.
async fn pending_obligations(api: &str, vault: &ObjectID) -> Result<HashMap<String, u64>> {
    let url = format!(
        "{}/trading-vaults/{}/pending-requests",
        api.trim_end_matches('/'),
        vault
    );
    let resp: PendingResp = reqwest::get(&url)
        .await
        .context("GET pending-requests")?
        .error_for_status()
        .context("pending-requests status")?
        .json()
        .await
        .context("decoding pending-requests")?;
    Ok(sum_obligations(resp.requests))
}

/// Cancel-replace one market's ladder around `mid`. Returns how many orders
/// now rest.
async fn requote(
    s: &Shared,
    ctx: &MarketCtx,
    open: &mut Vec<OpenOrder>,
    mid: u64,
) -> Result<usize> {
    let mut budget = quote_budget(s, ctx).await?;

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
            s.maker,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped configs must deserialize into `BotConfig` — schema
    /// drift (like the SO-372 pin fields this file used to require) fails
    /// here instead of at deploy.
    #[test]
    fn shipped_configs_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
        let staging = load_config(&dir.join("config.staging.toml")).unwrap();
        let vd = staging.vault_direct.expect("staging runs vault-direct");
        assert!(vd.vault_id.is_empty(), "staging must not pin a vault");
        assert!(vd.provision.enabled, "staging must self-provision");
        // SO-420: the tranche keys ride in via #[serde(flatten)], which
        // silently ignores unknown keys — a typo'd key would quietly fall
        // back to untranched. Pin the intent here.
        assert_eq!(vd.provision.tranche.structure_code, 1, "staging vault is tranched");
        assert_eq!(vd.provision.tranche.target_junior_bps, 2_000);
        load_config(&dir.join("config.toml")).unwrap();
    }

    /// Pin the SO-418 `/pending-requests` decode against the WS-3 wire
    /// contract: legacy fields camelCase, v2 additions snake_case, both
    /// on ONE object. Unknown fields (v2 adds several more) must be
    /// ignored, and blocked requests still count as obligations.
    #[test]
    fn pending_requests_v2_mixed_casing_decodes_and_sums() {
        let body = serde_json::json!({
            "requests": [{
                "seq": "7",
                "recipient": "0xabc",
                "sharesRaw": "1000000",
                "basisRaw": "250",
                "payoutCoinType": "0x2::tusdc::TUSDC",
                "payoutSymbol": "TUSDC",
                "requestedAtMs": "1",
                "global_seq": "7",
                "lane": "junior",
                "lane_code": 1,
                "position_id": "0xdef",
                "tranche": "junior",
                "tranche_code": 2,
                "capital_generation": 0,
                "payable": false,
                "blocked_reason": "junior_lane_blocked",
            }, {
                "seq": "8",
                "sharesRaw": "1",
                "basisRaw": "50",
                "payoutCoinType": "0x2::tusdc::TUSDC",
                "payoutSymbol": "TUSDC",
                "requestedAtMs": "2",
                "global_seq": "8",
                "lane": "senior",
                "lane_code": 0,
                "position_id": "0xfed",
                "tranche": "senior",
                "tranche_code": 1,
                "capital_generation": 0,
                "payable": true,
                "blocked_reason": null,
            }]
        });
        let resp: PendingResp = serde_json::from_value(body).unwrap();
        let out = sum_obligations(resp.requests);
        let canonical = canonicalize_move_type("0x2::tusdc::TUSDC").unwrap();
        // Blocked + payable both deduct: 250 + 50.
        assert_eq!(out.get(&canonical).copied(), Some(300));
    }
}
