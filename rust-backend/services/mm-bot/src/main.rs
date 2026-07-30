//! Market-maker bot — the vol desk (SO-299).
//!
//! Phase 0 (once per deployment): `mm-bot deploy-collateral` publishes this
//! MM's own copy of the `mm_collateral` package and persists
//! `{package_id, account_id, upgrade_cap}` (legacy chassis; the desk's
//! quotes route through the trading vault's `vault_mm` release instead).
//!
//! Phase 1: bootstrap — config + secrets, collateral routing, token-info
//! catalog, per-market vol buffers fed from the oracle-service WS price
//! cache, on-chain QuoteSigner (created + funded on first run).
//!
//! Phase 2: the desk (`mm_bot::desk`) — V1 delta-hedged long-vol fund
//! (V2 two-sided maker behind `[desk.v2]`): book reconstructed from VAULT
//! custody, limits engine, paper-hedged delta bands, on-chain auction
//! bidder, exit ladder, monitors + nightly stress. The WS serve loop
//! authenticates with the quoting service and prices RFQs through the
//! desk; every quote's collateral routing points at the TRADING VAULT
//! (`release_module = "vault_mm"`, outputs to the vault address) — the
//! bot is the vault's curator and nothing else.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use serde::Deserialize;
use sui_types::base_types::ObjectID;

use protocol_types::ids::{ObjectId as PtObjectId, SuiAddress as PtSuiAddress};
use protocol_types::messages::{
    AuthResponsePayload, BulkViewMmPremium, BulkViewQuotePayload, MmHelloPayload, MmQuotePayload,
    MmToService, ServiceToMm,
};
use protocol_types::quote::Quote;
use protocol_types::sides::MmRole;
use protocol_types::SigningScheme;

use api_service_client::ApiServiceClient;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use sui_tx::quote_signer::QuoteSigner;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::mm_collateral::balance_of as collateral_balance_of;
use sui_tx::tx::signer::{create_and_share_signer, find_signer, verify_signer};
use sui_tx::tx::test_tokens::mint_and_deposit_into_collateral;
use sui_tx::ws_client;
use token_info_client::{Snapshot, TokenInfoClient};

use mm_bot::collateral;
use mm_bot::desk::quote::{Decision, RfqInputs};
use mm_bot::liquidity::{FaucetLiquiditySource, LiquiditySource};
use mm_bot::pricing::{compute_spot_from_cache, serves_pair, Staleness};
use mm_bot::{Cli, Command};

// -- Config --------------------------------------------------------------

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:8084".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
struct BotConfig {
    /// HTTP health-check bind address. Defaults to `0.0.0.0:8084`.
    #[serde(default = "default_health_addr")]
    health_addr: std::net::SocketAddr,

    /// Sui network the bot operates on. Selects the deployments.json
    /// slot, the `[sui].<network>` secret slot, and the Sui RPC URL.
    network: Network,

    quoting_url: String,

    /// Quote-signing scheme. Stored on chain alongside the pubkey; the
    /// `MM_QUOTE_KEY` env var holds the 32-byte secret in this scheme.
    #[serde(default = "default_scheme")]
    signing_scheme: SigningScheme,

    /// This MM's published mm_collateral package id. Optional — when
    /// absent the bot reads the state file written by
    /// `mm-bot deploy-collateral`. Legacy chassis: the desk's quotes
    /// route through the vault instead, but the account still backs the
    /// signer bootstrap funding.
    #[serde(default)]
    collateral_package: Option<String>,
    /// The shared `CollateralAccount` object id, paired with
    /// `collateral_package`.
    #[serde(default)]
    collateral_account: Option<String>,

    /// This bot's existing `QuoteSigner` object id. Optional. When set it is
    /// verified against chain state (right deployment, right owner/key) and
    /// adopted; when absent — or when it fails verification — the bot falls
    /// back to discovering the signer from `SignerCreated` events.
    ///
    /// The fallback is history-dependent and can become unservable once the
    /// creating transaction ages out of the RPC provider (SO-325), which is
    /// what this field exists to route around. It is a hint, never a grant:
    /// a value that does not verify is ignored, not trusted.
    #[serde(default)]
    quote_signer_id: Option<String>,

    /// Explicit allowlist of underlyings to make markets in. Empty (the
    /// default) ⇒ derive mode: every enabled token-info token with a Pyth
    /// feed, with a watcher restart on new listings.
    #[serde(default = "default_underlying_symbols")]
    underlying_symbols: Vec<String>,

    /// Tickers to never market-make, even in derive mode.
    #[serde(default)]
    underlying_exclude: Vec<String>,

    /// Derive mode only: token-info re-poll cadence for new listings.
    #[serde(default = "default_underlying_refresh_secs")]
    underlying_refresh_secs: u64,

    #[serde(default = "default_settlement")]
    settlement_symbol: String,

    /// Annualized risk-free rate. Protocol convention is r = 0.
    #[serde(default)]
    rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,

    /// Roles advertised to the quoting service.
    roles: Vec<MmRole>,

    /// Opt in to answering unsigned bulk-view RFQs (indicative premiums
    /// for the frontend's tiles). Defaults to false.
    #[serde(default)]
    bulk_view_enabled: bool,

    /// On first run, mint+deposit this much settlement asset into the
    /// freshly-created Account so it can pay premiums.
    #[serde(default = "default_bootstrap_amount")]
    bootstrap_settlement_amount: u64,

    /// On first run, mint+deposit this much *underlying* asset into the
    /// freshly-created Account. In underlying smallest-units.
    #[serde(default = "default_bootstrap_underlying_amount")]
    bootstrap_underlying_amount: u64,

    /// Background top-up: when the Account's underlying balance falls below
    /// this, mint+deposit `underlying_replenish_amount` more. 0 disables.
    #[serde(default = "default_underlying_replenish_threshold")]
    underlying_replenish_threshold: u64,

    /// Amount minted+deposited on each auto-replenish top-up.
    #[serde(default = "default_underlying_replenish_amount")]
    underlying_replenish_amount: u64,

    /// How often the replenish task checks the underlying balance.
    #[serde(default = "default_replenish_interval_secs")]
    underlying_replenish_interval_secs: u64,

    /// Pyth staleness / vol-sampler settings. All fields have defaults.
    #[serde(default)]
    pyth: PythConfig,

    /// The vol desk (SO-299) — V1 long-vol fund, V2 behind `[desk.v2]`.
    #[serde(default)]
    desk: mm_bot::desk::DeskConfig,

    /// Testnet-only counterparty sim (SO-299): opens covered-call RFQ
    /// auctions as a retail stand-in + redeems expired positions.
    #[serde(default)]
    sim: mm_bot::sim::SimConfig,
}

/// Vol + staleness knobs for the live price cache fed from oracle-service.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PythConfig {
    /// Reject an RFQ if our last *observation* of either price is older
    /// than this. Catches a wedged or disconnected stream (oracle WS).
    max_price_age_ms: u64,
    /// Reject an RFQ if Pyth's publisher timestamp is older than this.
    max_publish_lag_ms: u64,
    /// Reject an RFQ if either feed's Pyth confidence interval exceeds this
    /// many basis points of its price. 0 disables.
    max_conf_bps: u64,
    /// Rolling window (hours) for the short realized-vol window.
    vol_window_hours: u64,
    /// Long realized-vol window (hours). Default 168 (7d).
    vol_long_window_hours: u64,
    /// How often the live cache is sampled into the vol buffers.
    vol_sample_interval_ms: u64,
    /// Volatility used until the buffer has enough samples.
    fallback_vol: f64,
    /// Per-symbol overrides for `fallback_vol` (e.g. `TBTC = 0.45`).
    fallback_vols: HashMap<String, f64>,
}

impl Default for PythConfig {
    fn default() -> Self {
        Self {
            max_price_age_ms: 5_000,
            max_publish_lag_ms: 10_000,
            max_conf_bps: 0,
            vol_window_hours: 24,
            vol_long_window_hours: 168,
            vol_sample_interval_ms: 300_000,
            fallback_vol: 0.6,
            fallback_vols: HashMap::new(),
        }
    }
}

fn default_scheme() -> SigningScheme {
    SigningScheme::Ed25519
}

fn default_underlying_symbols() -> Vec<String> {
    // Empty ⇒ derive the underlying set from token-info's enabled catalog.
    Vec::new()
}
fn default_underlying_refresh_secs() -> u64 {
    600
}
fn default_settlement() -> String {
    "TUSDC".into()
}
fn default_quote_ttl_ms() -> u64 {
    30_000
}
fn default_bootstrap_amount() -> u64 {
    1_000_000_000_000
} // 1e12 raw — plenty of settlement to quote with
fn default_bootstrap_underlying_amount() -> u64 {
    100_000_000_000
} // 1e11 raw underlying — inventory to write against
fn default_underlying_replenish_threshold() -> u64 {
    20_000_000_000
} // top up when underlying drops below 2e10 raw
fn default_underlying_replenish_amount() -> u64 {
    100_000_000_000
} // mint 1e11 raw per top-up
fn default_replenish_interval_secs() -> u64 {
    60
}

// -- Markets -------------------------------------------------------------

/// One underlying the bot makes markets in. Settlement is shared across all
/// markets, so only the underlying-specific context lives here.
struct Market {
    symbol: String,
    /// Canonical underlying coin type — the key a bucket's `asset_type` is
    /// matched against to pick this market.
    coin_type: String,
    feed: PriceFeedId,
    decimals: u8,
    /// Short-window realized-vol buffer fed from this underlying's USD price.
    vol_buf: Arc<RwLock<RollingVolBuffer>>,
    /// Long-window buffer (same samples).
    vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    /// Sigma used while the buffers are cold.
    fallback_vol: f64,
}

/// Derive the underlying set from token-info: every enabled token that has a
/// Pyth feed, excluding the settlement asset and any configured opt-outs.
fn derive_underlyings(snapshot: &Snapshot, settlement: &str, exclude: &[String]) -> Vec<String> {
    let mut out: Vec<String> = snapshot
        .tokens()
        .iter()
        .filter(|t| t.enabled && t.pyth_feed_id.is_some())
        .map(|t| t.ticker.clone())
        .filter(|tk| !tk.eq_ignore_ascii_case(settlement))
        .filter(|tk| !exclude.iter().any(|x| x.eq_ignore_ascii_case(tk)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Derive mode only: poll token-info and cleanly restart the process when a new
/// underlying is listed, so boot rebuilds the market set. Debounced — a new
/// underlying must appear on two consecutive polls before we restart.
fn spawn_underlying_watcher(
    token_info_url: String,
    booted: HashSet<String>,
    settlement: String,
    exclude: Vec<String>,
    interval_secs: u64,
) {
    tokio::spawn(async move {
        let mut pending: HashSet<String> = HashSet::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately; skip it so we don't re-derive the set
        // we just booted with.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let snapshot = match TokenInfoClient::new(&token_info_url).fetch().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "underlying watcher: token-info fetch failed; keeping current markets"
                    );
                    pending.clear();
                    continue;
                }
            };
            let current: HashSet<String> = derive_underlyings(&snapshot, &settlement, &exclude)
                .into_iter()
                .collect();
            // React to additions only — never restart on a removal or a blip.
            let new: HashSet<String> = current.difference(&booted).cloned().collect();
            if new.is_empty() {
                pending.clear();
                continue;
            }
            let confirmed: Vec<String> = new.intersection(&pending).cloned().collect();
            if !confirmed.is_empty() {
                let mut names = confirmed;
                names.sort();
                tracing::warn!(
                    new_underlyings = ?names,
                    "new underlying(s) listed in token-info — restarting to make markets in them"
                );
                std::process::exit(0);
            }
            let mut names: Vec<String> = new.iter().cloned().collect();
            names.sort();
            tracing::info!(
                observed = ?names,
                "underlying watcher: new underlying(s) seen; will restart if still present next poll"
            );
            pending = new;
        }
    });
}

// -- Main loop -----------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("mm-bot");

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    let secrets_loaded = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;

    // Subcommand: publish this MM's own mm_collateral package and exit.
    if let Some(Command::DeployCollateral { contracts, out }) = &cli.command {
        let wrap = SuiClientWrapper::connect(&secrets_loaded, cfg.network).await?;
        let dep = collateral::deploy(
            &wrap.client,
            &wrap.signer.keypair,
            wrap.signer.address,
            contracts,
            cfg.network.as_str(),
            cli.gas_budget,
        )
        .await?;
        let out = out
            .clone()
            .unwrap_or_else(|| collateral::default_state_path(cfg.network.as_str()));
        collateral::store(&out, &dep)?;
        println!("mm_collateral published:");
        println!("  package_id  = {}", dep.package_id);
        println!("  account_id  = {}", dep.account_id);
        println!("  upgrade_cap = {}", dep.upgrade_cap);
        println!("state persisted to {}", out.display());
        return Ok(());
    }

    let readiness = observability::ops::Readiness::new();
    observability::ops::spawn(cfg.health_addr, &readiness);

    // Collateral routing chassis: explicit config wins, else the state file
    // written by `mm-bot deploy-collateral`. Still required — the signer
    // bootstrap funds this account.
    let (collateral_package, collateral_account) = collateral::resolve(
        cfg.collateral_package.as_deref(),
        cfg.collateral_account.as_deref(),
        &collateral::state_path_candidates(cfg.network.as_str()),
        cfg.network.as_str(),
    )?;
    tracing::info!(
        %collateral_package,
        %collateral_account,
        "collateral routing resolved (signer bootstrap funding)"
    );
    // Resolve the token catalog from token-info. Hard cutover: if token-info
    // is unreachable after the retry window we crash.
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    let derive_mode = cfg.underlying_symbols.is_empty();
    let underlyings = if derive_mode {
        derive_underlyings(&snapshot, &cfg.settlement_symbol, &cfg.underlying_exclude)
    } else {
        cfg.underlying_symbols.clone()
    };
    if underlyings.is_empty() {
        anyhow::bail!(
            "no underlyings to make markets in ({})",
            if derive_mode {
                "token-info has no enabled, Pyth-fed, non-settlement tokens"
            } else {
                "underlying_symbols is empty"
            }
        );
    }
    tracing::info!(?underlyings, derive_mode, "resolved underlying set");
    let settlement_spec = snapshot.token_spec(&cfg.settlement_symbol).with_context(|| {
        format!(
            "settlement symbol {} not in token-info catalog",
            cfg.settlement_symbol
        )
    })?;
    let settlement_feed = settlement_spec.pyth_feed().with_context(|| {
        format!("missing pythFeedId for settlement {}", cfg.settlement_symbol)
    })?;
    let settlement_decimals = settlement_spec.decimals;
    let settlement_coin_type =
        protocol_types::asset::canonicalize_move_type(&settlement_spec.coin_type);

    // Build one Market per underlying. Vol buffers are created here; their
    // sampler tasks are spawned once the price cache is up (below).
    let vol_window_ms = cfg.pyth.vol_window_hours.saturating_mul(3_600_000);
    let vol_long_window_ms = cfg.pyth.vol_long_window_hours.saturating_mul(3_600_000);
    let mut markets: Vec<Market> = Vec::with_capacity(underlyings.len());
    for sym in &underlyings {
        let spec = snapshot
            .token_spec(sym)
            .with_context(|| format!("underlying symbol {sym} not in token-info catalog"))?;
        let feed = spec
            .pyth_feed()
            .with_context(|| format!("missing pythFeedId for underlying {sym}"))?;
        tracing::info!(underlying = %sym, %feed, decimals = spec.decimals, "market feed resolved");
        markets.push(Market {
            symbol: sym.clone(),
            coin_type: protocol_types::asset::canonicalize_move_type(&spec.coin_type),
            feed,
            decimals: spec.decimals,
            vol_buf: Arc::new(RwLock::new(RollingVolBuffer::new(vol_window_ms))),
            vol_buf_long: Arc::new(RwLock::new(RollingVolBuffer::new(vol_long_window_ms))),
            fallback_vol: cfg
                .pyth
                .fallback_vols
                .get(sym)
                .copied()
                .unwrap_or(cfg.pyth.fallback_vol),
        });
    }
    tracing::info!(
        markets = markets.len(),
        settlement = %cfg.settlement_symbol,
        settlement_feed = %settlement_feed,
        "pyth feeds resolved"
    );

    // Quote-signing key (scheme-aware) from the secrets TOML.
    let signer = load_quote_signer(&secrets_loaded, cfg.signing_scheme)?;
    let pubkey_bytes = signer.public_bytes();
    tracing::info!(scheme = ?signer.scheme(), pubkey_len = pubkey_bytes.len(), "quote signer ready");

    // QuoteSigner: bootstrap if missing; fund the MM's own CollateralAccount.
    let signer_id = resolve_signer(
        &cli,
        &cfg,
        &snapshot,
        &secrets_loaded,
        &signer,
        &pubkey_bytes,
        collateral_package,
        collateral_account,
    )
    .await?;
    tracing::info!(signer_id = %signer_id, "quote signer ready on chain");
    let signer_id_pt = pt_object_id_from_sui(signer_id);

    // Liquidity source: pulls settlement (and any coin the bot needs)
    // before quoting. Default = the test-token faucet.
    let liquidity: Arc<dyn LiquiditySource> = Arc::new(FaucetLiquiditySource::new(
        snapshot.maybe_test_tokens(),
        collateral_package,
        cli.gas_budget,
    ));

    // Keep each underlying's inventory topped up (writer-MM chassis).
    if cfg.roles.contains(&MmRole::WriterMm) && cfg.underlying_replenish_threshold > 0 {
        for sym in &underlyings {
            let underlying = match snapshot.faucet_token(sym) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        underlying = %sym,
                        error = %format!("{e:#}"),
                        "no faucet token; skipping auto-replenish for this underlying"
                    );
                    continue;
                }
            };
            spawn_replenish_task(ReplenishParams {
                secrets: secrets_loaded.clone(),
                network: cfg.network,
                collateral_package,
                collateral_account,
                coin_type: underlying.coin_type.clone(),
                symbol: sym.clone(),
                threshold: cfg.underlying_replenish_threshold,
                top_up: cfg.underlying_replenish_amount,
                interval_secs: cfg.underlying_replenish_interval_secs,
                liquidity: Arc::clone(&liquidity),
            });
        }
    }

    // Live prices come from oracle-service (the single Pyth gateway) over its
    // WS fanout.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    let mut all_feeds: Vec<PriceFeedId> = markets.iter().map(|m| m.feed).collect();
    all_feeds.push(settlement_feed);
    let (price_cache, _ws_task) = oracle.subscribe();

    // Maintain each market's rolling-vol buffers from the live cache.
    for m in &markets {
        spawn_vol_sampler(
            cfg.pyth.clone(),
            m.symbol.clone(),
            m.feed,
            price_cache.clone(),
            vec![Arc::clone(&m.vol_buf), Arc::clone(&m.vol_buf_long)],
        );
    }

    // Derive mode: watch token-info for newly-listed underlyings.
    if derive_mode {
        spawn_underlying_watcher(
            cli.token_info_url.clone(),
            underlyings.iter().cloned().collect(),
            cfg.settlement_symbol.clone(),
            cfg.underlying_exclude.clone(),
            cfg.underlying_refresh_secs,
        );
        tracing::info!(
            refresh_secs = cfg.underlying_refresh_secs,
            "underlying watcher started (derive mode)"
        );
    }

    // Don't enter the RFQ loop until every feed has produced at least one
    // observation.
    wait_for_first_prices(&price_cache, &all_feeds, Duration::from_secs(30)).await?;

    let protocol_id = snapshot.protocol_id_bytes()?;
    let api = ApiServiceClient::new(&cli.api_url);
    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
        max_conf_bps: cfg.pyth.max_conf_bps,
    };

    // ── the desk ────────────────────────────────────────────────────────
    // Vault-only maker: quotes release collateral from the trading vault
    // (`vault_mm`), auction winnings/exits land at the vault address.
    let mut desk: Option<Arc<mm_bot::desk::Desk>> = None;
    let mut vault_routing: Option<VaultRouting> = None;
    if cfg.desk.enabled {
        let trading_vault_package = snapshot
            .trading_vault()
            .context("trading_vault package missing from token-info (required by [desk])")?
            .package()?;
        let options_adapter_package = match snapshot.options_adapter() {
            Some(a) => Some(a.package()?),
            None => None,
        };
        let deepbook_adapter_package = match snapshot.deepbook_adapter() {
            Some(a) => Some(a.package()?),
            None => None,
        };
        // Shared governance objects the curator-session calls reference
        // (recorded by the deploy-time activation step, SO-292).
        let (integration_registry, pool_allowlist) = match snapshot.trading_vault_objects() {
            Some(o) => (Some(o.integration_registry()?), Some(o.pool_allowlist()?)),
            None => (None, None),
        };
        let (deepbook, deep_coin_type) = match snapshot.deepbook() {
            Some(db) => (
                Some(sui_tx::tx::deepbook::DeepBookHandles {
                    package: db.package()?,
                    original_package: db.original_package()?,
                    registry: db.registry()?,
                }),
                Some(db.deep_coin_type.clone()),
            ),
            None => (None, None),
        };
        let desk_markets = markets
            .iter()
            .map(|m| mm_bot::desk::DeskMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
            })
            .collect();
        let d = mm_bot::desk::spawn_desk(mm_bot::desk::DeskParams {
            cfg: cfg.desk.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            markets: desk_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            staleness,
            price_cache: price_cache.clone(),
            api_url: cli.api_url.clone(),
            indexer_url: cli.indexer_graphql_url.clone(),
            rate: cfg.rate,
            quote_ttl_ms: cfg.quote_ttl_ms,
            core_package: snapshot.package()?,
            trading_vault_package,
            options_adapter_package,
            deepbook_adapter_package,
            integration_registry,
            pool_allowlist,
            deepbook,
            deep_coin_type,
        })
        .await?;
        let vault_id = ObjectID::from_hex_literal(cfg.desk.vault_id.trim())
            .map_err(|e| anyhow!("bad [desk].vault_id: {e}"))?;
        vault_routing = Some(VaultRouting {
            collateral_source: pt_object_id_from_sui(vault_id),
            release_package: PtSuiAddress::new(
                *pt_object_id_from_sui(trading_vault_package).as_bytes(),
            ),
            release_module: "vault_mm".to_string(),
            signer_token_recipient: PtSuiAddress::new(*pt_object_id_from_sui(vault_id).as_bytes()),
        });
        desk = Some(d);
    } else {
        tracing::warn!("[desk] disabled — the bot serves health/auth only and declines every RFQ");
    }

    // Testnet counterparty sim (SO-299): opens covered-call RFQ auctions
    // + redeems expired positions. Independent of any maker loop now.
    if cfg.sim.enabled {
        mm_bot::sim::spawn_sim(mm_bot::sim::SimParams {
            cfg: cfg.sim.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            api_url: cli.api_url.clone(),
            core_package: snapshot.package()?,
            rfq_package: match snapshot.rfq() {
                Some(r) => Some(r.package()?),
                None => None,
            },
            settlement_coin_type: settlement_coin_type.clone(),
            liquidity: Arc::clone(&liquidity),
            has_faucets: snapshot.test_tokens().is_ok(),
            price_cache: price_cache.clone(),
            staleness,
            tokens: snapshot
                .tokens()
                .iter()
                .map(|t| mm_bot::sim::SimToken {
                    symbol: t.ticker.clone(),
                    coin_type: t.coin_type.clone(),
                    decimals: t.decimals,
                    feed: t
                        .pyth_feed_id
                        .as_deref()
                        .and_then(|f| protocol_types::PriceFeedId::from_hex(f).ok()),
                })
                .collect(),
        });
    }

    // Startup is done: collateral routing, the token-info snapshot, every
    // market's Pyth feed, the quote signer, the on-chain QuoteSigner bootstrap,
    // `wait_for_first_prices`, and the desk/sim spawns are all behind us. This
    // is the window mm-bot deployed green through on 2026-07-30 — /health was
    // live at the spawn above and the process died at `wait_for_first_prices`
    // half a second later, which the gate never saw (SO-324).
    //
    // DECLARED TAIL: the reconnect loop below is fallible and follows the flip.
    // Four exit paths, all in `main`'s own body — there is no `tokio::spawn` or
    // `async move` between here and the end of `main`, so each one kills the
    // process after /health has gone green:
    //
    //   AuthVerdict::Fatal          permanently-rejected key
    //   signer.sign(&challenge)?    auth handshake
    //   quote.to_bcs_bytes()?       steady-state, per-RFQ
    //   signer.sign(&bytes)?        steady-state, per-RFQ
    //
    // The first two are startup-shaped, and the flip deliberately does not wait
    // for them: that would make mm-bot's deploy gate depend on the
    // quoting-service being up, and this loop exists precisely to tolerate
    // transient rejection right after a redeploy while the indexer catches up.
    //
    // The last two are not startup at all — one BCS or signing failure on a
    // single quote exits the bot, and **no flip placement can cover them**,
    // because they live in an unbounded steady-state loop. That is what makes
    // this tail unavoidable rather than mis-placed. They also sit among
    // neighbours that deliberately warn-and-reconnect instead of exiting
    // (`ws_client::connect`, `expect_auth_challenge`), so the bare `?` there
    // looks unintended rather than chosen. Both are pre-existing and untouched
    // by SO-324; whether they should warn-and-continue is SO-328.
    //
    // Those last two are also *unreachable today*, and only because of a config
    // value: `signing_scheme = "ed25519"`, whose arm of `QuoteSigner::sign` has
    // no error path at all. The secp256k1 and secp256r1 arms do. So this is a
    // dormant exit rather than an unlikely one, and its trigger is a scheme
    // change rather than elapsed time — see the note at the call site.
    readiness.ready();

    // nonce is monotonic for the bot's lifetime — keep it across reconnects.
    let mut nonce_counter = now_ms();

    // Connect → authenticate → serve, reconnecting with capped exponential
    // backoff (transient auth rejections are expected right after a
    // redeploy while the indexer catches up).
    const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = INITIAL_BACKOFF;

    'reconnect: loop {
        let mut ws = match ws_client::connect(&cfg.quoting_url).await {
            Ok(ws) => ws,
            Err(e) => {
                tracing::warn!(error = %e, backoff_s = backoff.as_secs(),
                    "connect to quoting service failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue 'reconnect;
            }
        };

        // Auth handshake: Hello → AuthChallenge → AuthResponse → AuthAck.
        let hello = MmToService::Hello {
            payload: MmHelloPayload {
                roles: cfg.roles.clone(),
                account_id: signer_id_pt,
                signing_scheme: signer.scheme(),
                signing_pubkey: pubkey_bytes.clone(),
                bulk_view: cfg.bulk_view_enabled,
            },
        };
        if let Err(e) = ws_client::send_json(&mut ws, &hello).await {
            tracing::warn!(error = %e, "sending Hello failed; reconnecting");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue 'reconnect;
        }
        let challenge = match expect_auth_challenge(&mut ws).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "reading auth challenge failed; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue 'reconnect;
            }
        };
        let sig = signer.sign(&challenge)?;
        if let Err(e) = ws_client::send_json(
            &mut ws,
            &MmToService::AuthResponse {
                payload: AuthResponsePayload { signature: sig },
            },
        )
        .await
        {
            tracing::warn!(error = %e, "sending AuthResponse failed; reconnecting");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            continue 'reconnect;
        }
        match expect_auth_ack(&mut ws).await {
            Ok(AuthVerdict::Ok) => {
                tracing::info!("authenticated with quoting service");
                backoff = INITIAL_BACKOFF;
            }
            Ok(AuthVerdict::Retryable { code, message }) => {
                tracing::warn!(%code, %message, backoff_s = backoff.as_secs(),
                    "auth not ready yet (indexer catching up); retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue 'reconnect;
            }
            Ok(AuthVerdict::Fatal { code, message }) => {
                tracing::error!(%code, %message,
                    "auth permanently rejected — mm-bot signing key/scheme does not match the registered account");
                anyhow::bail!("auth permanently rejected: {code} — {message}");
            }
            Err(e) => {
                tracing::warn!(error = %e, "unexpected response during auth; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue 'reconnect;
            }
        }

        // Serve: price + sign RFQs until the connection drops, then reconnect.
        'serve: loop {
            let frame: ServiceToMm = match ws_client::next_json(&mut ws).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "ws closed; reconnecting");
                    break 'serve;
                }
            };
            let frame_type = match &frame {
                ServiceToMm::RFQBroadcast { .. } => "rfq_broadcast",
                ServiceToMm::BulkViewRFQBroadcast { .. } => "bulk_view_rfq_broadcast",
                ServiceToMm::Ping => "ping",
                _ => "other",
            };
            metrics::counter!("mm_bot_ws_messages_total", "type" => frame_type).increment(1);
            match frame {
                ServiceToMm::RFQBroadcast {
                    request_id,
                    payload,
                } => {
                    tracing::debug!(
                        ?request_id,
                        bucket_id = %payload.bucket_id,
                        write_amount = payload.write_amount,
                        "received rfq broadcast"
                    );
                    let rfq_start = std::time::Instant::now();
                    let now = now_ms();

                    let decline = |reason: String| MmToService::Decline {
                        request_id: request_id.clone(),
                        payload: protocol_types::messages::DeclinePayload { reason },
                    };

                    let (Some(desk_ref), Some(routing)) = (&desk, &vault_routing) else {
                        if let Err(e) =
                            ws_client::send_json(&mut ws, &decline("desk disabled".into())).await
                        {
                            tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                            break 'serve;
                        }
                        continue 'serve;
                    };

                    // Resolve the bucket's true pricing inputs from api-service
                    // by address — never trust the broadcast.
                    let bucket = match api.bucket_pricing(payload.bucket_id).await {
                        Ok(Some(b)) => b,
                        not_found_or_err => {
                            let reason = match not_found_or_err {
                                Ok(None) => "unknown or settled bucket".to_string(),
                                Err(e) => format!("bucket lookup failed: {e:#}"),
                                Ok(Some(_)) => unreachable!(),
                            };
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "bucket_lookup")
                                .increment(1);
                            tracing::debug!(?request_id, %reason, "declining");
                            if let Err(e) = ws_client::send_json(&mut ws, &decline(reason)).await {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    // Pick the market whose pair this bucket belongs to.
                    let Some(mi) = markets.iter().position(|m| {
                        serves_pair(
                            &bucket.asset_coin_type,
                            &bucket.settlement_coin_type,
                            &m.coin_type,
                            &settlement_coin_type,
                        )
                    }) else {
                        let reason = format!(
                            "pair not served: {}/{}",
                            bucket.asset_coin_type, bucket.settlement_coin_type
                        );
                        metrics::counter!("mm_bot_quote_failures_total", "reason" => "pair_not_served")
                            .increment(1);
                        tracing::debug!(?request_id, %reason, "declining");
                        if let Err(e) = ws_client::send_json(&mut ws, &decline(reason)).await {
                            tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                            break 'serve;
                        }
                        continue 'serve;
                    };

                    // Live spot scaled into the bucket's units.
                    let spot = match compute_spot_from_cache(
                        &price_cache,
                        markets[mi].feed,
                        settlement_feed,
                        markets[mi].decimals,
                        settlement_decimals,
                        staleness,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "stale_price")
                                .increment(1);
                            tracing::debug!(?request_id, reason = e.as_str(), "declining: stale market data");
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &decline(format!("stale market data: {}", e.as_str())),
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    let inputs = RfqInputs {
                        write_amount: payload.write_amount,
                        is_put: bucket.is_put,
                        strike: bucket.strike,
                        strike_scale: bucket.strike_scale,
                        expiry_ms: bucket.expiry_ms,
                    };
                    match desk_ref
                        .price_ws_rfq(payload.side, mi, inputs, spot, true, now)
                        .await
                    {
                        Decision::Quote { premium } => {
                            nonce_counter = nonce_counter.wrapping_add(1);
                            // Vault-only routing: collateral from the vault's
                            // `vault_mm` release; outputs to the vault.
                            let quote = Quote {
                                protocol_id: protocol_id.clone(),
                                signer_id: signer_id_pt,
                                collateral_source: routing.collateral_source,
                                release_package: routing.release_package,
                                release_module: routing.release_module.clone(),
                                signer_token_recipient: routing.signer_token_recipient,
                                bucket_id: payload.bucket_id,
                                write_amount: payload.write_amount,
                                premium,
                                valid_until_ms: now.saturating_add(cfg.quote_ttl_ms),
                                nonce: nonce_counter,
                            };
                            // These two `?`s exit the process — see the DECLARED
                            // TAIL note at the readiness flip. They are
                            // unreachable *only because of the configured
                            // signing scheme*: `QuoteSigner::sign` has no error
                            // path in its Ed25519 arm (quote_signer.rs), and
                            // `to_bcs_bytes` is `bcs::to_bytes` over a struct of
                            // primitives. Set `signing_scheme` to secp256k1 or
                            // secp256r1 — both of which `sign_prehash(..)?` —
                            // and this becomes a live exit on one bad quote,
                            // mid-session, long after /health went green. Fix
                            // before switching scheme: warn → backoff →
                            // `continue 'reconnect`, matching the neighbours
                            // above. Tracked in SO-328.
                            let bytes = quote.to_bcs_bytes()?;
                            let sig = signer.sign(&bytes)?;
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Quote {
                                    request_id,
                                    payload: MmQuotePayload {
                                        quote,
                                        signature: sig,
                                    },
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (quote) failed; reconnecting");
                                break 'serve;
                            }
                            metrics::counter!("mm_bot_quotes_signed_total").increment(1);
                            metrics::histogram!("mm_bot_rfq_response_duration_seconds")
                                .record(rfq_start.elapsed().as_secs_f64());
                            tracing::info!(premium, nonce = nonce_counter, "quote sent");
                        }
                        Decision::Decline { reason } => {
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "price_declined")
                                .increment(1);
                            tracing::debug!(?request_id, %reason, "declining");
                            if let Err(e) = ws_client::send_json(&mut ws, &decline(reason)).await {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                        }
                    }
                }
                ServiceToMm::BulkViewRFQBroadcast {
                    request_id,
                    payload,
                } => {
                    tracing::debug!(
                        ?request_id,
                        buckets = payload.bucket_ids.len(),
                        write_amount = payload.write_amount,
                        "received bulk-view rfq broadcast"
                    );
                    let now = now_ms();
                    let mut premiums = Vec::with_capacity(payload.bucket_ids.len());
                    if let Some(desk_ref) = &desk {
                        // One spot read per market for the whole batch; `None`
                        // where that market's feed is currently stale.
                        let spots: Vec<Option<f64>> = markets
                            .iter()
                            .map(|m| {
                                compute_spot_from_cache(
                                    &price_cache,
                                    m.feed,
                                    settlement_feed,
                                    m.decimals,
                                    settlement_decimals,
                                    staleness,
                                )
                                .ok()
                            })
                            .collect();
                        for bucket_id in &payload.bucket_ids {
                            let bucket = match api.bucket_pricing(*bucket_id).await {
                                Ok(Some(b)) => b,
                                Ok(None) => continue,
                                Err(e) => {
                                    tracing::debug!(bucket_id = %bucket_id, error = %format!("{e:#}"), "bulk-view: bucket lookup failed; skipping");
                                    continue;
                                }
                            };
                            let Some((mi, spot)) = markets
                                .iter()
                                .position(|m| {
                                    serves_pair(
                                        &bucket.asset_coin_type,
                                        &bucket.settlement_coin_type,
                                        &m.coin_type,
                                        &settlement_coin_type,
                                    )
                                })
                                .and_then(|i| spots[i].map(|s| (i, s)))
                            else {
                                continue;
                            };
                            let inputs = RfqInputs {
                                write_amount: payload.write_amount,
                                is_put: bucket.is_put,
                                strike: bucket.strike,
                                strike_scale: bucket.strike_scale,
                                expiry_ms: bucket.expiry_ms,
                            };
                            // Indicative only: nothing is signed, no nonce is
                            // burned, no premium is reserved.
                            if let Decision::Quote { premium } = desk_ref
                                .price_ws_rfq(payload.side, mi, inputs, spot, false, now)
                                .await
                            {
                                premiums.push(BulkViewMmPremium {
                                    bucket_id: *bucket_id,
                                    premium,
                                });
                            }
                        }
                    }
                    if let Err(e) = ws_client::send_json(
                        &mut ws,
                        &MmToService::BulkViewQuote {
                            request_id,
                            payload: BulkViewQuotePayload { premiums },
                        },
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "ws send (bulk-view quote) failed; reconnecting");
                        break 'serve;
                    }
                }
                ServiceToMm::Ping => {
                    if let Err(e) = ws_client::send_json(&mut ws, &MmToService::Pong).await {
                        tracing::warn!(error = %e, "ws send (pong) failed; reconnecting");
                        break 'serve;
                    }
                }
                other => {
                    tracing::debug!(?other, "ignored frame");
                }
            }
        }

        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

// -- helpers -------------------------------------------------------------

/// The vault-only collateral routing baked into every signed quote
/// (product decision, doc 05: the bot trades only as the vault's curator).
struct VaultRouting {
    collateral_source: PtObjectId,
    release_package: PtSuiAddress,
    release_module: String,
    signer_token_recipient: PtSuiAddress,
}

fn load_config(path: &Path) -> Result<BotConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::from(path).required(true))
        .build()
        .with_context(|| format!("loading {}", path.display()))?;
    let cfg: BotConfig = settings
        .try_deserialize()
        .with_context(|| format!("parsing {}", path.display()))?;
    tracing::debug!(
        network = %cfg.network,
        underlyings = ?cfg.underlying_symbols,
        settlement = %cfg.settlement_symbol,
        scheme = ?cfg.signing_scheme,
        roles = ?cfg.roles,
        quote_ttl_ms = cfg.quote_ttl_ms,
        rate = cfg.rate,
        desk = cfg.desk.enabled,
        "bot config loaded"
    );
    Ok(cfg)
}

fn load_quote_signer(
    secrets: &runtime_config::Secrets,
    scheme: SigningScheme,
) -> Result<QuoteSigner> {
    QuoteSigner::from_secret_str(secrets.mm_quote_key()?, scheme)
}

/// Resolve (or bootstrap) the bot's on-chain QuoteSigner and, on a fresh
/// bootstrap, fund the MM's own CollateralAccount so it can quote on day one.
#[allow(clippy::too_many_arguments)]
async fn resolve_signer(
    cli: &Cli,
    cfg: &BotConfig,
    snapshot: &token_info_client::Snapshot,
    secrets: &runtime_config::Secrets,
    signer: &QuoteSigner,
    pubkey_bytes: &[u8],
    collateral_package: ObjectID,
    collateral_account: ObjectID,
) -> Result<ObjectID> {
    let wrap = SuiClientWrapper::connect(secrets, cfg.network).await?;
    let package = snapshot.package()?;

    // Chain state is the source of truth either way; the only question is how
    // we look. A configured id is checked with a point read of the object —
    // cheap, and immune to the archival retention that makes the event scan
    // below fail (SO-325). It is a hint, not a grant: `verify_signer` proves
    // the object is a QuoteSigner of THIS deployment owned by THIS bot with
    // THIS key before we adopt it, so a stale or wrong id falls through
    // rather than being trusted.
    if let Some(configured) = cfg.quote_signer_id.as_deref() {
        let signer_id =
            ObjectID::from_hex_literal(configured).context("parsing quote_signer_id")?;
        if verify_signer(
            &wrap.client,
            package,
            signer_id,
            wrap.signer.address,
            signer.scheme(),
            pubkey_bytes,
        )
        .await?
        {
            tracing::info!(%signer_id, "adopted configured quote signer (verified on chain)");
            return Ok(signer_id);
        }
        tracing::warn!(
            %signer_id,
            "configured quote_signer_id did not verify — falling back to event discovery"
        );
    }

    // No usable configured id. If this bot's Sui address already created a
    // QuoteSigner under the current package, adopt it; otherwise bootstrap a
    // fresh one.
    if let Some(signer_id) =
        find_signer(&wrap.client, package, wrap.signer.address, signer.scheme(), pubkey_bytes)
            .await?
    {
        tracing::info!(%signer_id, "adopted existing on-chain quote signer for this deployment");
        return Ok(signer_id);
    }

    tracing::info!("no quote signer for the current deployment — bootstrapping a fresh one");
    let created = create_and_share_signer(
        &wrap.client,
        &wrap.signer,
        package,
        signer.scheme(),
        pubkey_bytes,
        cli.gas_budget,
    )
    .await?;
    tracing::info!(digest = %created.digest, signer_id = %created.signer_id, "quote signer created");

    // Fund the MM's own CollateralAccount with settlement so it can pay
    // premiums on day one. Create and fund are separate txs; a crash
    // between them leaves the signer (adopted on the next boot) unfunded —
    // acceptable for the test MM bot.
    let settlement = snapshot.faucet_token(&cfg.settlement_symbol)?;
    let (tokens_pkg, settlement_module) = settlement.module_path()?;
    let fund_resp = mint_and_deposit_into_collateral(
        &wrap.client,
        &wrap.signer,
        tokens_pkg,
        &settlement_module,
        settlement.faucet()?,
        &settlement.coin_type,
        collateral_account,
        collateral_package,
        cfg.bootstrap_settlement_amount,
        cli.gas_budget,
    )
    .await?;
    tracing::info!(
        digest = %fund_resp.digest,
        amount = cfg.bootstrap_settlement_amount,
        symbol = %cfg.settlement_symbol,
        "collateral account funded (settlement)"
    );

    // Fund it with each underlying so it can write calls to retail traders.
    for sym in &cfg.underlying_symbols {
        let underlying = snapshot.faucet_token(sym)?;
        let (u_tokens_pkg, underlying_module) = underlying.module_path()?;
        let fund_resp = mint_and_deposit_into_collateral(
            &wrap.client,
            &wrap.signer,
            u_tokens_pkg,
            &underlying_module,
            underlying.faucet()?,
            &underlying.coin_type,
            collateral_account,
            collateral_package,
            cfg.bootstrap_underlying_amount,
            cli.gas_budget,
        )
        .await?;
        tracing::info!(
            digest = %fund_resp.digest,
            amount = cfg.bootstrap_underlying_amount,
            symbol = %sym,
            "collateral account funded (underlying)"
        );
    }

    Ok(created.signer_id)
}

async fn expect_auth_challenge(ws: &mut ws_client::WsStream) -> Result<Vec<u8>> {
    match ws_client::next_json::<ServiceToMm>(ws).await? {
        ServiceToMm::AuthChallenge { payload } => Ok(payload.challenge),
        other => Err(anyhow!("expected AuthChallenge, got {:?}", other)),
    }
}

/// Classified outcome of the auth handshake's final ack.
enum AuthVerdict {
    /// Authenticated — proceed to serve.
    Ok,
    /// The quoting service rejected auth for a reason that resolves on its
    /// own — chiefly `auth_scheme_unknown` (the indexer hasn't ingested our
    /// SignerCreated yet). Retry until it catches up.
    Retryable { code: String, message: String },
    /// A permanent rejection (`auth_pubkey_mismatch` / `auth_signature_invalid`):
    /// the registered key/scheme will never match what we present.
    Fatal { code: String, message: String },
}

/// Read the ack frame and classify it. `Err` covers a ws error or an
/// unexpected frame — the caller treats those as a transient disconnect.
async fn expect_auth_ack(ws: &mut ws_client::WsStream) -> Result<AuthVerdict> {
    match ws_client::next_json::<ServiceToMm>(ws).await? {
        ServiceToMm::AuthAck { .. } => Ok(AuthVerdict::Ok),
        ServiceToMm::Error { payload, .. } => {
            let code = payload.code;
            let message = payload.message;
            // Only these two are permanent; everything else (incl. the
            // expected `auth_scheme_unknown`) is worth retrying.
            match code.as_str() {
                "auth_pubkey_mismatch" | "auth_signature_invalid" => {
                    Ok(AuthVerdict::Fatal { code, message })
                }
                _ => Ok(AuthVerdict::Retryable { code, message }),
            }
        }
        other => Err(anyhow!("expected AuthAck, got {:?}", other)),
    }
}

fn pt_object_id_from_sui(id: ObjectID) -> PtObjectId {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(id.into_bytes().as_ref());
    PtObjectId::new(bytes)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Block until every feed has at least one cached observation (ignoring the
/// publish-lag bound). Times out so a misconfigured feed id surfaces quickly
/// rather than hanging the bot forever.
async fn wait_for_first_prices(
    cache: &PriceCache,
    feeds: &[PriceFeedId],
    timeout: Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if let Some(missing) = feeds.iter().find(|f| cache.peek(**f).is_none()) {
            if start.elapsed() > timeout {
                return Err(anyhow!(
                    "pyth: no observation within {:?} for feed {missing}",
                    timeout
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            tracing::info!(feeds = feeds.len(), "pyth: first prices observed for all feeds");
            return Ok(());
        }
    }
}

/// Inputs for the underlying-inventory replenish task.
struct ReplenishParams {
    secrets: runtime_config::Secrets,
    network: Network,
    /// The MM's own mm_collateral package + shared CollateralAccount.
    collateral_package: ObjectID,
    collateral_account: ObjectID,
    coin_type: String,
    symbol: String,
    threshold: u64,
    top_up: u64,
    interval_secs: u64,
    /// Source the top-up is pulled from (faucet by default).
    liquidity: Arc<dyn LiquiditySource>,
}

/// Periodically read the CollateralAccount's underlying balance (via
/// devInspect, no gas) and mint+deposit a top-up when it drops below the
/// configured threshold.
fn spawn_replenish_task(p: ReplenishParams) {
    tokio::spawn(async move {
        let wrap = match SuiClientWrapper::connect(&p.secrets, p.network).await {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "replenish: failed to connect; task exiting");
                return;
            }
        };
        let mut ticker = tokio::time::interval(Duration::from_secs(p.interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let balance = match collateral_balance_of(
                &wrap.client,
                wrap.signer.address,
                p.collateral_package,
                p.collateral_account,
                &p.coin_type,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "replenish: balance read failed; retrying next tick");
                    continue;
                }
            };
            if balance >= p.threshold {
                tracing::trace!(balance, threshold = p.threshold, "replenish: inventory ok");
                continue;
            }
            tracing::info!(
                balance,
                threshold = p.threshold,
                top_up = p.top_up,
                symbol = %p.symbol,
                "replenish: underlying below threshold; minting top-up"
            );
            if p
                .liquidity
                .ensure_account_balance(
                    &wrap.client,
                    &wrap.signer,
                    p.collateral_account,
                    &p.coin_type,
                    p.top_up,
                )
                .await
            {
                tracing::info!(amount = p.top_up, symbol = %p.symbol, "replenish: topped up underlying");
            }
        }
    });
}

#[cfg(test)]
mod config_tests {
    use super::*;

    fn parse(name: &str) -> BotConfig {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join(name);
        load_config(&path).unwrap_or_else(|e| panic!("{name}: {e:#}"))
    }

    #[test]
    fn shipped_configs_parse_with_desk_defaults() {
        for name in ["config.toml", "config.staging.toml", "config.prod.toml"] {
            let cfg = parse(name);
            // An enabled desk must be fully wired to its provisioned
            // vault (staging is live per SO-299); envs without one ship
            // disabled.
            if cfg.desk.enabled {
                assert!(!cfg.desk.vault_id.is_empty(), "{name}: enabled desk needs vault_id");
                assert!(cfg.desk.mm_release_enabled, "{name}: enabled desk needs mm release");
            }
            // Defaults are the 00-plan starting parameters.
            assert_eq!(cfg.desk.limits.premium_budget_hard, 0.35, "{name}");
            assert_eq!(cfg.desk.v1.base_spread_volpts, 0.05, "{name}");
            assert!(!cfg.desk.v2.enabled, "{name}: v2 must ship disabled");
        }
        // Prod has no provisioned vault yet — desk stays off there.
        assert!(!parse("config.prod.toml").desk.enabled, "prod desk must ship disabled");
        assert!(parse("config.staging.toml").sim.enabled);
        assert!(!parse("config.prod.toml").sim.enabled);
    }
}

/// Maintain one market's vol buffers from the live price cache on the
/// configured cadence.
fn spawn_vol_sampler(
    cfg: PythConfig,
    symbol: String,
    feed: PriceFeedId,
    cache: PriceCache,
    bufs: Vec<Arc<RwLock<RollingVolBuffer>>>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(cfg.vol_sample_interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut vol_log_counter: u64 = 0;
        loop {
            ticker.tick().await;
            let Some(cp) = cache.peek(feed) else {
                continue;
            };
            // Pyth publisher age as seen at this sample (now - publish_time).
            let age_s = (now_ms() as i64).saturating_sub(cp.publish_time_ms) as f64 / 1000.0;
            metrics::gauge!("mm_bot_pyth_price_age_seconds", "symbol" => symbol.clone())
                .set(age_s);
            // Only sample if we recently observed something — avoid
            // re-pushing a stale price during a stream outage.
            if cp.observed_at.elapsed() > Duration::from_millis(cfg.max_price_age_ms) {
                continue;
            }
            let now = now_ms();
            for buf in &bufs {
                buf.write().push(now, cp.price);
            }
            if let Some(sigma) = bufs.first().and_then(|b| b.read().current_annualized()) {
                vol_log_counter += 1;
                if vol_log_counter % 60 == 1 {
                    let samples = bufs.first().map(|b| b.read().len()).unwrap_or(0);
                    tracing::debug!(sigma, samples, "vol updated");
                }
            }
        }
    });
}
