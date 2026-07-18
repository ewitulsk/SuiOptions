//! Market-maker bot.
//!
//! Phase 0 (once per deployment): `mm-bot deploy-collateral` publishes this
//! MM's own copy of the `mm_collateral` package (collateral abstraction,
//! plan §8) and persists `{package_id, account_id, upgrade_cap}`.
//!
//! Phase 1: bootstrap.
//!   - Reads its TOML config (incl. `signing_scheme`) + `MM_QUOTE_KEY`
//!     (32-byte hex secret — interpretation depends on the scheme).
//!   - Resolves the collateral routing: `collateral_package` /
//!     `collateral_account` from the config, else the deploy-collateral
//!     state file.
//!   - Resolves its QuoteSigner from chain state for the *current*
//!     deployment: looks up the `SignerCreated` event under the current
//!     package for this bot's Sui address. If none exists (e.g. right after
//!     a fresh contract deployment), calls
//!     `quote_signer::create_and_share_signer(scheme, pubkey)` and funds the
//!     MM's own CollateralAccount with `bootstrap_settlement_amount` via
//!     `test_tokens::<sym>::mint` + `mm_collateral::deposit`.
//!
//! Phase 2: serve.
//!   - Authenticates over WS via the scheme-aware challenge (§5.4.1).
//!   - Loops on `RFQBroadcast`, prices each option via Black-Scholes using
//!     the spot/vol/rate config, signs the BCS-encoded Quote (which carries
//!     the collateral routing INSIDE the signed payload) with the configured
//!     scheme, sends. Pongs Pings.
//!
//! The MM serves as a **Trader MM** by default (pays premium, receives the
//! call token). `roles` in the TOML controls advertised roles to the
//! quoting service.

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

use pricing::smile::Smile;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use api_service_client::ApiServiceClient;
use token_info_client::{Snapshot, TokenInfoClient};
use sui_tx::quote_signer::QuoteSigner;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::mm_collateral::balance_of as collateral_balance_of;
use sui_tx::tx::signer::{create_and_share_signer, find_signer};
use sui_tx::tx::test_tokens::mint_and_deposit_into_collateral;
use sui_tx::ws_client;

use mm_bot::collateral;
use mm_bot::liquidity::{FaucetLiquiditySource, LiquiditySource};
use mm_bot::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, serves_pair, PriceDecision, PricingConfig,
    RfqPricingInputs, SigmaEstimate, Staleness,
};
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
    /// One of `ed25519` / `secp256k1` / `secp256r1`.
    #[serde(default = "default_scheme")]
    signing_scheme: SigningScheme,

    /// This MM's published mm_collateral package id (the quote's
    /// `release_package`). Optional — when absent (the default) the bot
    /// reads the state file written by `mm-bot deploy-collateral`.
    #[serde(default)]
    collateral_package: Option<String>,
    /// The shared `CollateralAccount` object id (the quote's
    /// `collateral_source`). Optional, paired with `collateral_package`.
    #[serde(default)]
    collateral_account: Option<String>,
    /// Module holding the standardized `release` function inside
    /// `collateral_package` (the quote's `release_module`). Defaults to the
    /// first-party template's module name.
    #[serde(default = "default_release_module")]
    release_module: String,

    /// Explicit allowlist of underlyings to make markets in. Each symbol is
    /// looked up in the token-info catalog (coin type, decimals, `pythFeedId`)
    /// and quoted against the shared `settlement_symbol`.
    ///
    /// Empty (the default) ⇒ **derive mode**: the bot market-makes every
    /// enabled token-info token that has a Pyth feed and isn't the settlement
    /// asset, and a watcher restarts the bot to pick up newly-listed
    /// underlyings (see `underlying_refresh_secs`). Non-empty ⇒ pin exactly
    /// these and never auto-pick-up.
    #[serde(default = "default_underlying_symbols")]
    underlying_symbols: Vec<String>,

    /// Tickers to never market-make, even in derive mode (e.g. stablecoins or
    /// assets we list but don't quote). Case-insensitive. The settlement asset
    /// is always excluded automatically.
    #[serde(default)]
    underlying_exclude: Vec<String>,

    /// Derive mode only: how often to re-fetch token-info and check for a
    /// newly-listed underlying. A new underlying confirmed across two
    /// consecutive polls triggers a clean restart so boot rebuilds the market
    /// set. Default 600s (10 min).
    #[serde(default = "default_underlying_refresh_secs")]
    underlying_refresh_secs: u64,

    #[serde(default = "default_settlement")]
    settlement_symbol: String,

    /// Annualized risk-free rate. Protocol convention is r = 0 (the serde
    /// default): settlement is a stablecoin with no funded rate leg, and
    /// r = 0 keeps fair value identical across keeper / api-service /
    /// vault-sim / this bot. It also makes European put pricing exact for
    /// the American-exercisable on-chain puts.
    #[serde(default)]
    rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,

    /// Ask-side *minimum* markup in basis points of premium, applied when
    /// quoting as the Writer MM (retail buying — trader flow). The vol-space
    /// spread (`ask_vol_markup`) usually dominates; this is the floor left
    /// deep ITM where vega ≈ 0. Defaults to 100 (1%).
    #[serde(default = "default_spread_bps")]
    ask_markup_bps: u64,
    /// Bid-side *minimum* markdown in basis points of premium, applied when
    /// quoting as the Trader MM (retail writing — writer flow). Defaults to
    /// 100 (1%).
    #[serde(default = "default_spread_bps")]
    bid_markdown_bps: u64,
    /// Vol-space ask spread: sigma multiplier (≥ 1) when we sell options.
    /// Defaults to 1.0 (disabled) so unconfigured deployments keep the
    /// bps-only behavior.
    #[serde(default = "default_vol_spread_neutral")]
    ask_vol_markup: f64,
    /// Vol-space bid spread: sigma multiplier (≤ 1) when we buy options.
    /// Defaults to 1.0 (disabled).
    #[serde(default = "default_vol_spread_neutral")]
    bid_vol_markdown: f64,
    /// Last-look charge multiplier on `|delta|·spot·sigma·√(ttl_years)`,
    /// added to the ask / shaded off the bid. Defaults to 0.0 (disabled).
    #[serde(default)]
    ttl_charge_mult: f64,
    /// Extra vol widening (≥ 1) while quoting on the fallback sigma (cold
    /// vol buffer). Defaults to 1.0 (disabled).
    #[serde(default = "default_vol_spread_neutral")]
    fallback_vol_penalty: f64,
    /// Default vol smile (skew/convexity in standardized log-moneyness z —
    /// see `pricing::smile`). Flat by default; calibrate before enabling.
    #[serde(default)]
    smile: SmileConfig,
    /// Per-symbol smile overrides, e.g. `[smiles.TBTC] skew = 0.05`.
    #[serde(default)]
    smiles: HashMap<String, SmileConfig>,
    /// Decline any RFQ whose notional (spot × write_amount, settlement
    /// smallest-units) exceeds this. Defaults to 0 (no cap).
    #[serde(default)]
    max_quote_notional: u64,
    /// Size widening: extra proportional vol widening per
    /// `size_ref_notional` of quote notional. Defaults to 0.0 (disabled).
    #[serde(default)]
    size_widening_vol: f64,
    /// Reference notional (settlement smallest-units) for `size_widening_vol`.
    #[serde(default)]
    size_ref_notional: u64,

    /// Roles advertised to the quoting service.
    roles: Vec<MmRole>,

    /// Opt in to answering unsigned bulk-view RFQs (indicative premiums for
    /// the frontend's tiles). These are priced but never signed — no nonce is
    /// consumed and nothing reaches the chain. Defaults to false.
    #[serde(default)]
    bulk_view_enabled: bool,

    /// Where minted call tokens / position Objects should land. Defaults to
    /// the bot's Sui address.
    #[serde(default)]
    token_recipient: Option<String>,

    /// On first run, mint+deposit this much settlement asset into the
    /// freshly-created Account so it can pay premiums.
    #[serde(default = "default_bootstrap_amount")]
    bootstrap_settlement_amount: u64,

    /// On first run, mint+deposit this much *underlying* asset into the
    /// freshly-created Account so it can write calls to retail traders
    /// (writer-MM / ask side). In underlying smallest-units.
    #[serde(default = "default_bootstrap_underlying_amount")]
    bootstrap_underlying_amount: u64,

    /// Background top-up: when the Account's underlying balance falls below
    /// this, mint+deposit `underlying_replenish_amount` more. Set to 0 to
    /// disable auto-replenish.
    #[serde(default = "default_underlying_replenish_threshold")]
    underlying_replenish_threshold: u64,

    /// Amount minted+deposited on each auto-replenish top-up.
    #[serde(default = "default_underlying_replenish_amount")]
    underlying_replenish_amount: u64,

    /// How often the replenish task checks the underlying balance.
    #[serde(default = "default_replenish_interval_secs")]
    underlying_replenish_interval_secs: u64,

    /// Pyth Hermes/Benchmarks settings. All fields have defaults.
    #[serde(default)]
    pyth: PythConfig,

    /// DeepBook quoting loop (SO-158). Off by default; needs the network to
    /// carry a DeepBook deployment in token-info.
    #[serde(default)]
    deepbook: mm_bot::deepbook::DeepBookQuoterConfig,

    /// Trading-vault DeepBook quoting (SO-291): trade a curated vault's
    /// DeepBook custody through the deepbook-adapter curator calls instead
    /// of the bot's own BalanceManager. Off by default; mutually exclusive
    /// with `[deepbook]` (vault mode wins). Cadence / sizing / batching
    /// knobs are reused from the `[deepbook]` section.
    #[serde(default)]
    trading_vault: mm_bot::vault_deepbook::TradingVaultConfig,

    /// On-chain RFQ bidder (doc 05 Â§3) â the buy side of the vault's
    /// weekly call-slice auctions. Off by default.
    #[serde(default)]
    onchain_rfq: mm_bot::onchain_rfq::OnchainRfqConfig,

    /// On-chain cash-secured-PUT RFQ bidder — the put mirror of
    /// `[onchain_rfq]` (same config shape). Off by default.
    #[serde(default)]
    onchain_put_rfq: mm_bot::onchain_rfq::OnchainRfqConfig,

    /// On-chain proceeds-swap bidder (doc 05 §3.1) — the buy side of the
    /// vault's settlement→underlying swap auctions. Off by default.
    #[serde(default)]
    onchain_swap: mm_bot::onchain_swap::OnchainSwapConfig,
}

/// Vol + staleness knobs for the live price cache fed from oracle-service.
/// Prices/vol now come from oracle-service; these tune the consumer-side
/// guards and the rolling-vol sampler.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PythConfig {
    /// Reject an RFQ if our last *observation* of either price is older
    /// than this. Catches a wedged or disconnected stream (oracle WS).
    max_price_age_ms: u64,
    /// Reject an RFQ if Pyth's publisher timestamp is older than this.
    /// Catches the case where the stream is alive but Pyth itself isn't
    /// publishing.
    max_publish_lag_ms: u64,
    /// Reject an RFQ if either feed's Pyth confidence interval exceeds this
    /// many basis points of its price — a fresh feed that is unsure of
    /// itself is exactly when quotes get picked off. 0 disables.
    max_conf_bps: u64,
    /// Rolling window (in hours) used to compute realized vol — the short,
    /// regime-tracking window.
    vol_window_hours: u64,
    /// Long realized-vol window (in hours). The quoted sigma is the max of
    /// the two windows, so one calm day can't undercut what the trailing
    /// week actually realized. Default 168 (7d).
    vol_long_window_hours: u64,
    /// How often the live cache is sampled into the vol buffer. The vol
    /// estimate annualizes from the samples' actual timestamps, so skipped
    /// ticks (stale stream) don't bias it.
    vol_sample_interval_ms: u64,
    /// Volatility used until the buffer has enough samples. Once it does,
    /// the live estimate takes over. Overridable per symbol below.
    fallback_vol: f64,
    /// Per-symbol overrides for `fallback_vol` (e.g. `TBTC = 0.45`): one
    /// flat number is wrong in both directions for a majors/small-cap mix.
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

/// Serde mirror of [`pricing::smile::Smile`] (the pricing crate stays
/// serde-free). Defaults to flat.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct SmileConfig {
    skew: f64,
    convexity: f64,
}

impl From<SmileConfig> for Smile {
    fn from(c: SmileConfig) -> Self {
        Smile { skew: c.skew, convexity: c.convexity }
    }
}

fn default_scheme() -> SigningScheme {
    SigningScheme::Ed25519
}

fn default_release_module() -> String {
    "mm_collateral".into()
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
fn default_spread_bps() -> u64 {
    100
} // 1% minimum markup/markdown off the BS mid
fn default_vol_spread_neutral() -> f64 {
    1.0
} // sigma multiplier of 1.0 = vol-space spread disabled
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
/// markets (every bucket settles in the configured `settlement_symbol`), so
/// only the underlying-specific pricing context lives here.
struct Market {
    symbol: String,
    /// Canonical underlying coin type — the key a bucket's `asset_type` is
    /// matched against to pick this market.
    coin_type: String,
    feed: PriceFeedId,
    decimals: u8,
    /// Short-window realized-vol buffer fed from this underlying's USD price.
    vol_buf: Arc<RwLock<RollingVolBuffer>>,
    /// Long-window buffer (same samples); quoted sigma is max(short, long).
    vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    /// Sigma used while `vol_buf` is cold: the per-symbol override from
    /// `[pyth].fallback_vols`, else the global `fallback_vol`.
    fallback_vol: f64,
    /// Vol smile for this underlying: the per-symbol override from
    /// `[smiles]`, else the global `[smile]`.
    smile: Smile,
}

/// Derive the underlying set from token-info: every enabled token that has a
/// Pyth feed, excluding the settlement asset and any configured opt-outs.
/// Sorted + deduped for stable logging and set comparison.
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
/// underlying is listed, so boot rebuilds the market set (the Pyth subscription
/// and per-market tasks are fixed at boot, so a live add isn't possible).
/// Debounced — a new underlying must appear on two consecutive polls before we
/// restart, so a token-info blip never flaps the bot. Removals and fetch
/// failures never trigger a restart.
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
            // React to additions only — never restart on a removal or a blip
            // that drops the set.
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
                // Clean exit; the container restart policy reboots us and boot
                // rebuilds the full market set + Pyth subscription.
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

    observability::ops::spawn(cfg.health_addr);

    // Collateral routing (plan §8): explicit config wins, else the state file
    // written by `mm-bot deploy-collateral`. Required — quotes carry the
    // routing inside the signed payload.
    let (collateral_package, collateral_account) = collateral::resolve(
        cfg.collateral_package.as_deref(),
        cfg.collateral_account.as_deref(),
        &collateral::state_path_candidates(cfg.network.as_str()),
        cfg.network.as_str(),
    )?;
    tracing::info!(
        %collateral_package,
        %collateral_account,
        release_module = %cfg.release_module,
        "collateral routing resolved"
    );
    // Resolve the token catalog from token-info. Hard cutover: if token-info
    // is unreachable after the retry window we crash (no deployments.json
    // fallback).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    // /tokens catalog lookup (coin type, decimals, pyth feed). This is the
    // source the pricing path reads from; the bootstrap path separately looks
    // up the test-token faucet via `snapshot.faucet_token(symbol)`.
    //
    // Underlying set: an explicit `underlying_symbols` allowlist, or — when
    // empty — derived from token-info's enabled catalog (with a watcher that
    // restarts the bot to pick up new listings).
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
    // sampler tasks are spawned once the Pyth subscriber is up (below).
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
            smile: cfg.smiles.get(sym).copied().unwrap_or(cfg.smile).into(),
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
    let collateral_account_pt = pt_object_id_from_sui(collateral_account);
    let release_package_pt = PtSuiAddress::new(*pt_object_id_from_sui(collateral_package).as_bytes());

    // Liquidity source: pulls settlement (and, via the same trait, any coin the
    // bot needs) before quoting. Default = the test-token faucet; a real market
    // maker swaps in their own funding source at this one site.
    let liquidity: Arc<dyn LiquiditySource> = Arc::new(FaucetLiquiditySource::new(
        snapshot.maybe_test_tokens(),
        collateral_package,
        cli.gas_budget,
    ));

    // Keep each underlying's inventory topped up so the writer-MM (ask) side
    // never runs dry mid-test. One task per underlying. Only relevant if we
    // advertise writer_mm and auto-replenish is enabled.
    if cfg.roles.contains(&MmRole::WriterMm) && cfg.underlying_replenish_threshold > 0 {
        for sym in &underlyings {
            // A derived underlying might not be a faucet/test token; skip
            // auto-replenish for it rather than failing boot.
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
    // WS fanout. `subscribe()` returns a PriceCache a background task keeps
    // current; the hot RFQ path reads it with the same `get_fresh` staleness
    // check as when mm-bot owned the SSE connection itself.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    let mut all_feeds: Vec<PriceFeedId> = markets.iter().map(|m| m.feed).collect();
    all_feeds.push(settlement_feed);
    let (price_cache, _ws_task) = oracle.subscribe();

    // Maintain each market's rolling-vol buffer from the live cache on the
    // configured cadence. No Benchmarks bootstrap: the buffer warms from the
    // stream within a few samples and `fallback_vol` covers the brief
    // cold-start window.
    for m in &markets {
        spawn_vol_sampler(
            cfg.pyth.clone(),
            m.symbol.clone(),
            m.feed,
            price_cache.clone(),
            vec![Arc::clone(&m.vol_buf), Arc::clone(&m.vol_buf_long)],
        );
    }

    // Derive mode: watch token-info for newly-listed underlyings and restart to
    // pick them up. No-op when underlyings were pinned explicitly.
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

    // Don't enter the RFQ loop until every feed (all underlyings + settlement)
    // has produced at least one observation. Otherwise early RFQs decline for
    // stale data.
    wait_for_first_prices(&price_cache, &all_feeds, Duration::from_secs(30)).await?;

    // RFQ pricing context — built once, reused across reconnects.
    let token_recipient = resolve_token_recipient(&cfg, &secrets_loaded)?;
    let protocol_id = snapshot.protocol_id_bytes()?;
    let pricing_cfg = PricingConfig {
        rate: cfg.rate,
        quote_ttl_ms: cfg.quote_ttl_ms,
        ask_markup_bps: cfg.ask_markup_bps,
        bid_markdown_bps: cfg.bid_markdown_bps,
        ask_vol_markup: cfg.ask_vol_markup,
        bid_vol_markdown: cfg.bid_vol_markdown,
        ttl_charge_mult: cfg.ttl_charge_mult,
        fallback_vol_penalty: cfg.fallback_vol_penalty,
        smile: cfg.smile.into(),
        max_quote_notional: cfg.max_quote_notional,
        size_widening_vol: cfg.size_widening_vol,
        size_ref_notional: cfg.size_ref_notional,
    };
    // api-service client: the bot looks each RFQ's bucket up by address to get
    // its true (strike, expiry, coin types) rather than trusting the broadcast.
    let api = ApiServiceClient::new(&cli.api_url);
    tracing::info!(
        api_url = %cli.api_url,
        markets = ?cfg.underlying_symbols,
        settlement = %cfg.settlement_symbol,
        "bucket lookups via api-service; quoting these underlyings"
    );
    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
        max_conf_bps: cfg.pyth.max_conf_bps,
    };

    // Trading-vault DeepBook quoting (SO-291): same quoting brain as the
    // plain quoter below, but the orders rest in a curated vault's DeepBook
    // custody via the deepbook-adapter curator calls. Mutually exclusive with
    // `[deepbook]` — vault mode wins if both are enabled.
    if cfg.trading_vault.enabled && cfg.deepbook.enabled {
        tracing::error!(
            "[deepbook] and [trading_vault] quoters are mutually exclusive; preferring trading-vault mode"
        );
    }
    if cfg.trading_vault.enabled {
        let adapter_package = snapshot
            .deepbook_adapter()
            .context("deepbook-adapter package missing from token-info (required by [trading_vault])")?
            .package()?;
        let tv_objects = snapshot
            .trading_vault_objects()
            .context("trading-vault objects missing from token-info (required by [trading_vault])")?;
        let quoter_markets = markets
            .iter()
            .map(|m| mm_bot::deepbook::QuoterMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
                smile: m.smile,
            })
            .collect();
        mm_bot::vault_deepbook::spawn_quoter(mm_bot::vault_deepbook::VaultQuoterParams {
            cfg: cfg.trading_vault.clone(),
            db_cfg: cfg.deepbook.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            adapter_package,
            integration_registry: tv_objects.integration_registry()?,
            pool_allowlist: tv_objects.pool_allowlist()?,
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: quoter_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            pricing: pricing_cfg,
            staleness,
        });
        tracing::info!(
            vault = %cfg.trading_vault.vault_id,
            "trading-vault deepbook quoting enabled"
        );
    }

    // DeepBook quoting loop (SO-158): rest two-sided limit orders on every
    // tradeable bucket pool of the configured markets, priced by the same
    // Black-Scholes path that answers RFQs (one QuoterMarket per Market,
    // sharing its vol buffer — SO-159).
    if cfg.deepbook.enabled && !cfg.trading_vault.enabled {
        match snapshot.deepbook() {
            Some(db) => {
                let handles = sui_tx::tx::deepbook::DeepBookHandles {
                    package: db.package()?,
                    original_package: db.original_package()?,
                    registry: db.registry()?,
                };
                let quoter_markets = markets
                    .iter()
                    .map(|m| mm_bot::deepbook::QuoterMarket {
                        symbol: m.symbol.clone(),
                        coin_type: m.coin_type.clone(),
                        feed: m.feed,
                        decimals: m.decimals,
                        vol_buf: Arc::clone(&m.vol_buf),
                        vol_buf_long: Arc::clone(&m.vol_buf_long),
                        fallback_vol: m.fallback_vol,
                        smile: m.smile,
                    })
                    .collect();
                mm_bot::deepbook::spawn_quoter(mm_bot::deepbook::QuoterParams {
                    cfg: cfg.deepbook.clone(),
                    secrets: secrets_loaded.clone(),
                    network: cfg.network,
                    handles,
                    api_url: cli.api_url.clone(),
                    price_cache: price_cache.clone(),
                    markets: quoter_markets,
                    settlement_feed,
                    settlement_coin_type: settlement_coin_type.clone(),
                    settlement_decimals,
                    pricing: pricing_cfg,
                    staleness,
                    liquidity: Arc::clone(&liquidity),
                });
                tracing::info!(markets = cfg.underlying_symbols.len(), "deepbook quoting enabled");
            }
            None => tracing::warn!(
                "deepbook.enabled set but token-info reports no DeepBook deployment; quoting disabled"
            ),
        }
    }

    // The on-chain bidders bid through the generic `auction` package
    // (four-package split); resolve it once, failing boot if any bidder
    // is enabled on a deployment without it.
    let auction_package = if cfg.onchain_rfq.enabled
        || cfg.onchain_put_rfq.enabled
        || cfg.onchain_swap.bidder.enabled
    {
        Some(
            snapshot
                .auction()
                .context("auction package missing from token-info (required by the on-chain bidders)")?
                .package()?,
        )
    } else {
        None
    };

    // On-chain RFQ bidder (C2): poll open auctions, price them with the
    // same brain, bid from the wallet under the escrow cap.
    if cfg.onchain_rfq.enabled {
        let bidder_markets = markets
            .iter()
            .map(|m| mm_bot::onchain_rfq::BidderMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
                smile: m.smile,
            })
            .collect();
        mm_bot::onchain_rfq::spawn_bidder(mm_bot::onchain_rfq::BidderParams {
            cfg: cfg.onchain_rfq.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            package: auction_package.expect("resolved above"),
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: bidder_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            pricing: pricing_cfg,
            staleness,
        });
        tracing::info!("onchain rfq bidder enabled");
    }

    // On-chain cash-secured-PUT RFQ bidder: poll open put auctions, price them
    // with the put leg of the same brain, bid the premium from the wallet under
    // the escrow cap (same accounting as the call bidder).
    if cfg.onchain_put_rfq.enabled {
        let bidder_markets = markets
            .iter()
            .map(|m| mm_bot::onchain_put_rfq::BidderMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
                smile: m.smile,
            })
            .collect();
        mm_bot::onchain_put_rfq::spawn_bidder(mm_bot::onchain_put_rfq::BidderParams {
            cfg: cfg.onchain_put_rfq.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            package: auction_package.expect("resolved above"),
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: bidder_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            pricing: pricing_cfg,
            staleness,
        });
        tracing::info!("onchain put rfq bidder enabled");
    }

    // On-chain swap bidder: the buy side of the vault's proceeds-swap
    // auctions (settlement → underlying), discovered straight from
    // AuctionCreated events.
    if cfg.onchain_swap.bidder.enabled {
        let swap_markets = markets
            .iter()
            .map(|m| mm_bot::onchain_rfq::BidderMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
                smile: m.smile,
            })
            .collect();
        mm_bot::onchain_swap::spawn_bidder(mm_bot::onchain_swap::SwapBidderParams {
            cfg: cfg.onchain_swap.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            package: auction_package.expect("resolved above"),
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: swap_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            staleness,
        });
        tracing::info!("onchain swap bidder enabled");
    }

    // nonce is monotonic for the bot's lifetime — keep it across reconnects.
    let mut nonce_counter = now_ms();

    // Connect → authenticate → serve, reconnecting with capped exponential
    // backoff. A transient auth rejection — the indexer hasn't ingested our
    // SignerCreated yet (`auth_scheme_unknown`) — or a dropped connection is
    // expected right after a redeploy, so we keep the process (and its
    // /health endpoint) alive and retry until the indexer catches up. Only a
    // permanent auth error (a key/scheme mismatch the indexer will never
    // accept) is fatal.
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

                    // Resolve the bucket's true pricing inputs from api-service
                    // by address. The broadcast carries no strike/expiry/pair, so
                    // a spoofed or buggy upstream can't trick us into mispricing.
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
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Decline {
                                    request_id,
                                    payload: protocol_types::messages::DeclinePayload { reason },
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    // Pick the market whose pair this bucket belongs to. None
                    // means we don't source a spot for it — decline.
                    let market = match find_market(&markets, &bucket, &settlement_coin_type) {
                        Some(m) => m,
                        None => {
                            let reason = format!(
                                "pair not served: {}/{}",
                                bucket.asset_coin_type, bucket.settlement_coin_type
                            );
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "pair_not_served")
                                .increment(1);
                            tracing::debug!(?request_id, %reason, "declining");
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Decline {
                                    request_id,
                                    payload: protocol_types::messages::DeclinePayload { reason },
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    // Live spot from Pyth for this market, scaled into the
                    // bucket's units (settlement smallest-units per underlying
                    // smallest-unit).
                    let spot_scaled = match compute_spot_from_cache(
                        &price_cache,
                        market.feed,
                        settlement_feed,
                        market.decimals,
                        settlement_decimals,
                        staleness,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            let reason: &'static str = e.as_str();
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "stale_price")
                                .increment(1);
                            tracing::debug!(?request_id, reason, "declining: stale market data");
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Decline {
                                    request_id,
                                    payload: protocol_types::messages::DeclinePayload {
                                        reason: format!("stale market data: {reason}"),
                                    },
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };
                    let sigma = resolve_sigma(
                        market.vol_buf.read().current_annualized(),
                        market.vol_buf_long.read().current_annualized(),
                        market.fallback_vol,
                    );

                    let inputs = RfqPricingInputs {
                        write_amount: payload.write_amount,
                        side: payload.side,
                        strike: bucket.strike,
                        strike_scale: bucket.strike_scale,
                        expiry_ms: bucket.expiry_ms,
                        is_put: bucket.is_put,
                    };
                    let market_cfg = PricingConfig { smile: market.smile, ..pricing_cfg };
                    match price_rfq(&market_cfg, &inputs, spot_scaled, sigma, now) {
                        PriceDecision::Quote {
                            premium,
                            valid_until_ms,
                            spot_scaled,
                            strike_scaled,
                            t_years,
                            sigma,
                            per_unit,
                        } => {
                            tracing::debug!(
                                market = %market.symbol,
                                spot = spot_scaled,
                                sigma,
                                strike = strike_scaled,
                                strike_raw = %bucket.strike,
                                strike_scale = bucket.strike_scale,
                                t_years,
                                per_unit,
                                write_amount = payload.write_amount,
                                premium,
                                "priced"
                            );
                            nonce_counter = nonce_counter.wrapping_add(1);
                            let quote = Quote {
                                protocol_id: protocol_id.clone(),
                                signer_id: signer_id_pt,
                                collateral_source: collateral_account_pt,
                                release_package: release_package_pt,
                                release_module: cfg.release_module.clone(),
                                signer_token_recipient: token_recipient,
                                bucket_id: payload.bucket_id,
                                write_amount: payload.write_amount,
                                premium,
                                valid_until_ms,
                                nonce: nonce_counter,
                            };
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
                        PriceDecision::Decline { reason } => {
                            metrics::counter!("mm_bot_quote_failures_total", "reason" => "price_declined")
                                .increment(1);
                            tracing::debug!(?request_id, %reason, "declining");
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Decline {
                                    request_id,
                                    payload: protocol_types::messages::DeclinePayload { reason },
                                },
                            )
                            .await
                            {
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
                    // One spot/vol read per market for the whole batch; `None`
                    // where that market's feed is currently stale.
                    let spots: Vec<Option<(f64, SigmaEstimate, Smile)>> = markets
                        .iter()
                        .map(|m| {
                            match compute_spot_from_cache(
                                &price_cache,
                                m.feed,
                                settlement_feed,
                                m.decimals,
                                settlement_decimals,
                                staleness,
                            ) {
                                Ok(spot) => Some((
                                    spot,
                                    resolve_sigma(
                                        m.vol_buf.read().current_annualized(),
                                        m.vol_buf_long.read().current_annualized(),
                                        m.fallback_vol,
                                    ),
                                    m.smile,
                                )),
                                Err(_) => None,
                            }
                        })
                        .collect();

                    let mut premiums = Vec::with_capacity(payload.bucket_ids.len());
                    for bucket_id in &payload.bucket_ids {
                        // Resolve each bucket from api-service (cached); skip ones
                        // we can't price for any reason — a bulk-view bucket has
                        // no per-bucket decline.
                        let bucket = match api.bucket_pricing(*bucket_id).await {
                            Ok(Some(b)) => b,
                            Ok(None) => continue,
                            Err(e) => {
                                tracing::debug!(bucket_id = %bucket_id, error = %format!("{e:#}"), "bulk-view: bucket lookup failed; skipping");
                                continue;
                            }
                        };
                        // Match the bucket to one of our markets and grab that
                        // market's spot/sigma; skip if unserved or stale.
                        let Some((spot_scaled, sigma, smile)) = markets
                            .iter()
                            .position(|m| {
                                serves_pair(
                                    &bucket.asset_coin_type,
                                    &bucket.settlement_coin_type,
                                    &m.coin_type,
                                    &settlement_coin_type,
                                )
                            })
                            .and_then(|i| spots[i])
                        else {
                            continue;
                        };
                        // Reuse the signed-RFQ pricer; we keep only the premium —
                        // no Quote is built, no nonce burned, nothing is signed.
                        let inputs = RfqPricingInputs {
                            write_amount: payload.write_amount,
                            side: payload.side,
                            strike: bucket.strike,
                            strike_scale: bucket.strike_scale,
                            expiry_ms: bucket.expiry_ms,
                            is_put: bucket.is_put,
                        };
                        let market_cfg = PricingConfig { smile, ..pricing_cfg };
                        if let PriceDecision::Quote { premium, .. } =
                            price_rfq(&market_cfg, &inputs, spot_scaled, sigma, now)
                        {
                            premiums.push(BulkViewMmPremium {
                                bucket_id: *bucket_id,
                                premium,
                            });
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

    // The deployment is the source of truth — no local sidecar. If this
    // bot's Sui address already created a QuoteSigner under the current
    // package, adopt it; otherwise bootstrap a fresh one. A fresh contract
    // deployment (new package) has no such event, so the bot self-heals by
    // creating a new signer against the package the indexer is watching.
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
    // premiums on day one (Trader-MM / bid side). Create and fund are
    // separate txs; a crash between them leaves the signer (adopted on the
    // next boot) unfunded — acceptable for the test MM bot.
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

    // Fund it with each underlying so it can write calls to retail traders
    // (Writer-MM / ask side). The background replenish tasks keep these topped
    // up as the inventory drains.
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
    /// the registered key/scheme will never match what we present. Fatal —
    /// retrying can't fix a misconfigured key.
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

fn resolve_token_recipient(
    cfg: &BotConfig,
    secrets: &runtime_config::Secrets,
) -> Result<PtSuiAddress> {
    if let Some(s) = &cfg.token_recipient {
        tracing::debug!(recipient = %s, "using configured token recipient");
        return PtSuiAddress::from_hex(s).context("parsing token_recipient");
    }
    tracing::debug!("deriving token recipient from sui key");
    // Derive the address from the same Sui key the bot signs gas with.
    let raw = secrets.sui_private_key(cfg.network.as_str())?;
    let kp = sui_types::crypto::SuiKeyPair::decode(raw.trim())
        .map_err(|e| anyhow!("decoding sui key: {e}"))?;
    let addr = sui_types::base_types::SuiAddress::from(&kp.public());
    PtSuiAddress::from_hex(&addr.to_string()).context("converting sui address")
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

/// Pick the market whose pair matches this bucket. `None` when no configured
/// market quotes the bucket's `(underlying, settlement)` pair.
fn find_market<'a>(
    markets: &'a [Market],
    bucket: &api_service_client::BucketPricing,
    settlement_coin_type: &str,
) -> Option<&'a Market> {
    markets.iter().find(|m| {
        serves_pair(
            &bucket.asset_coin_type,
            &bucket.settlement_coin_type,
            &m.coin_type,
            settlement_coin_type,
        )
    })
}

/// Inputs for the underlying-inventory replenish task.
struct ReplenishParams {
    secrets: runtime_config::Secrets,
    network: Network,
    /// The MM's own mm_collateral package + shared CollateralAccount — the
    /// bot tracks its own available funds by RPC-reading its own account
    /// (plan §8: its own concern, not protocol infrastructure).
    collateral_package: ObjectID,
    collateral_account: ObjectID,
    coin_type: String,
    symbol: String,
    threshold: u64,
    top_up: u64,
    interval_secs: u64,
    /// Source the top-up is pulled from (faucet by default). The faucet id /
    /// module / gas are resolved inside the source from `coin_type`.
    liquidity: Arc<dyn LiquiditySource>,
}

/// Periodically read the CollateralAccount's underlying balance (via
/// devInspect, no gas) and mint+deposit a top-up when it drops below the
/// configured threshold. Runs in its own tokio task with its own Sui client
/// so it doesn't contend with the WS serve loop. Transient errors are logged
/// and retried on the next tick — a wedged faucet shouldn't kill the bot.
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

/// Maintain one market's vol buffer from the live price cache on the
/// configured cadence. The buffer warms from the live stream (no Benchmarks
/// bootstrap); `fallback_vol` covers the cold-start window.
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
