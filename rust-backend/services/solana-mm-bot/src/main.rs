//! Market-maker bot for the Solana options protocol — the port of
//! `services/mm-bot`.
//!
//! Phase 1: bootstrap.
//!   - Reads its TOML config + secrets (`[solana]` wallet keypair,
//!     `[mm_bot].quote_key` — a 32-byte ed25519 seed, hex or base58).
//!   - Resolves the token catalog + program info from solana-token-info.
//!   - Ensures the MmAccount PDA exists (`create_account(salt, scheme=0,
//!     quote_pubkey)` — the PDA is deterministic, so no event walking),
//!     ensures wallet ATAs, and deposits inventory into the MmAccount when
//!     it sits below the configured floors (minting to the wallet via the
//!     solana-gas-station faucet on non-mainnet).
//!
//! Phase 2: serve.
//!   - Authenticates to solana-quoting-service over WS (sign the 32-byte
//!     challenge with the ed25519 quote key).
//!   - Loops on `RFQBroadcast`: resolve the bucket from solana-api-service,
//!     price via Black-Scholes (vol-space spreads / smile / ttl charge —
//!     the shared brain in `pricing.rs`), sign the Borsh quote bytes, send.
//!     Unsigned `BulkViewRFQBroadcast`s are priced but never signed.
//!   - The unified auction bidder (covered_call / cash_secured_put / swap)
//!     runs beside the WS flow when enabled.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ed25519_dalek::Signer as _;
use parking_lot::RwLock;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;

use protocol_types::sides::MmRole;
use pyth_client::{PriceCache, PriceFeedId, RollingVolBuffer};
use solana_token_info_client::{Snapshot, TokenInfoClient};
use solana_tx::quote::{sign_quote, Quote, QuoteWire};
use solana_tx::SolanaClientWrapper;

use solana_mm_bot::api_client::ApiClient;
use solana_mm_bot::auction::{self, BidderMode, OnchainAuctionConfig};
use solana_mm_bot::bootstrap;
use solana_mm_bot::messages::{
    AuthResponsePayload, BulkViewMmPremium, BulkViewQuotePayload, DeclinePayload, MmHelloPayload,
    MmQuotePayload, MmToService, ServiceToMm,
};
use solana_mm_bot::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, serves_pair, PriceDecision, PricingConfig,
    RfqPricingInputs, SigmaEstimate, Smile, Staleness,
};
use solana_mm_bot::ws_client;
use solana_mm_bot::Cli;

// -- Config --------------------------------------------------------------

fn default_health_addr() -> std::net::SocketAddr {
    "0.0.0.0:9010".parse().unwrap()
}

#[derive(Debug, Clone, Deserialize)]
struct BotConfig {
    /// HTTP ops (health/metrics) bind address. Defaults to `0.0.0.0:9010`.
    #[serde(default = "default_health_addr")]
    health_addr: std::net::SocketAddr,

    /// Solana cluster the bot operates on. Selects the `[solana].<network>`
    /// secret slot and the default RPC URL.
    network: solana_tx::Network,

    quoting_url: String,

    /// solana-indexer GraphQL endpoint — auction discovery for the
    /// on-chain bidders.
    #[serde(default = "default_indexer_graphql_url")]
    indexer_graphql_url: String,

    /// solana-gas-station base URL for the non-mainnet test-token faucet.
    /// Unset ⇒ never mint; inventory only moves what the wallet holds.
    #[serde(default)]
    faucet_url: Option<String>,

    /// MmAccount PDA salt (multiple accounts per wallet). Default 0.
    #[serde(default)]
    mm_account_salt: u64,

    /// Explicit allowlist of underlyings to make markets in. Each symbol is
    /// looked up in the solana-token-info catalog (mint, decimals,
    /// `pythFeedId`) and quoted against the shared `settlement_symbol`.
    ///
    /// Empty (the default) ⇒ **derive mode**: the bot market-makes every
    /// enabled token-info token that has a Pyth feed and isn't the settlement
    /// asset, and a watcher restarts the bot to pick up newly-listed
    /// underlyings (see `underlying_refresh_secs`). Non-empty ⇒ pin exactly
    /// these and never auto-pick-up.
    #[serde(default)]
    underlying_symbols: Vec<String>,

    /// Tickers to never market-make, even in derive mode. Case-insensitive.
    /// The settlement asset is always excluded automatically.
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
    /// r = 0 keeps fair value identical across the stack. It also makes
    /// European put pricing exact for the American-exercisable on-chain puts.
    #[serde(default)]
    rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,

    /// Ask-side *minimum* markup in basis points of premium (Writer-MM
    /// side). The vol-space spread usually dominates; this is the floor
    /// left deep ITM where vega ≈ 0. Defaults to 100 (1%).
    #[serde(default = "default_spread_bps")]
    ask_markup_bps: u64,
    /// Bid-side *minimum* markdown in basis points of premium (Trader-MM
    /// side). Defaults to 100 (1%).
    #[serde(default = "default_spread_bps")]
    bid_markdown_bps: u64,
    /// Vol-space ask spread: sigma multiplier (≥ 1) when we sell options.
    #[serde(default = "default_vol_spread_neutral")]
    ask_vol_markup: f64,
    /// Vol-space bid spread: sigma multiplier (≤ 1) when we buy options.
    #[serde(default = "default_vol_spread_neutral")]
    bid_vol_markdown: f64,
    /// Last-look charge multiplier on `|delta|·spot·sigma·√(ttl_years)`.
    #[serde(default)]
    ttl_charge_mult: f64,
    /// Extra vol widening (≥ 1) while quoting on the fallback sigma.
    #[serde(default = "default_vol_spread_neutral")]
    fallback_vol_penalty: f64,
    /// Default vol smile (skew/convexity in standardized log-moneyness z).
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
    /// `size_ref_notional` of quote notional.
    #[serde(default)]
    size_widening_vol: f64,
    /// Reference notional (settlement smallest-units) for `size_widening_vol`.
    #[serde(default)]
    size_ref_notional: u64,

    /// Roles advertised to the quoting service.
    roles: Vec<MmRole>,

    /// Opt in to answering unsigned bulk-view RFQs (indicative premiums for
    /// the frontend's tiles). Priced but never signed — no nonce consumed.
    #[serde(default)]
    bulk_view_enabled: bool,

    /// Wallet that receives minted option tokens on quote execution
    /// (`Quote.signer_token_recipient`, base58). Defaults to the bot's
    /// wallet address.
    #[serde(default)]
    token_recipient: Option<String>,

    /// MmAccount settlement inventory floor + top-up (smallest-units): the
    /// bot deposits `bootstrap_settlement_amount` from the wallet whenever
    /// the MmAccount's settlement ATA drops below it (Trader-MM / bid side
    /// pays premiums from here). 0 disables.
    #[serde(default = "default_bootstrap_amount")]
    bootstrap_settlement_amount: u64,

    /// MmAccount underlying inventory deposited at boot per underlying
    /// (Writer-MM / ask side writes collateral from here). 0 disables.
    #[serde(default = "default_bootstrap_underlying_amount")]
    bootstrap_underlying_amount: u64,

    /// Background top-up: when an underlying's MmAccount balance falls
    /// below this, deposit `underlying_replenish_amount` more. 0 disables.
    #[serde(default = "default_underlying_replenish_threshold")]
    underlying_replenish_threshold: u64,

    /// Amount deposited on each auto-replenish top-up.
    #[serde(default = "default_underlying_replenish_amount")]
    underlying_replenish_amount: u64,

    /// How often the replenish task checks the balances.
    #[serde(default = "default_replenish_interval_secs")]
    underlying_replenish_interval_secs: u64,

    /// Pyth staleness guards + rolling-vol sampler knobs (prices come from
    /// solana-oracle-service). All fields have defaults.
    #[serde(default)]
    pyth: PythConfig,

    /// Unified on-chain auction bidder (covered_call / cash_secured_put /
    /// swap modes). Off by default.
    #[serde(default)]
    onchain_auction: OnchainAuctionConfig,
}

/// Vol + staleness knobs for the live price cache fed from
/// solana-oracle-service — same names as the Sui twin so operators
/// recognize them.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PythConfig {
    /// Reject an RFQ if our last *observation* of either price is older
    /// than this. Catches a wedged or disconnected oracle WS stream.
    max_price_age_ms: u64,
    /// Reject an RFQ if Pyth's publisher timestamp is older than this.
    max_publish_lag_ms: u64,
    /// Reject an RFQ if either feed's Pyth confidence interval exceeds this
    /// many basis points of its price. 0 disables.
    max_conf_bps: u64,
    /// Rolling window (hours) for the short realized-vol estimate.
    vol_window_hours: u64,
    /// Long realized-vol window (hours); quoted sigma is max(short, long).
    vol_long_window_hours: u64,
    /// How often the live cache is sampled into the vol buffer.
    vol_sample_interval_ms: u64,
    /// Volatility used until the buffer has enough samples.
    fallback_vol: f64,
    /// Per-symbol overrides for `fallback_vol`.
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

fn default_indexer_graphql_url() -> String {
    "http://127.0.0.1:9002/graphql".into()
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
} // deposit 1e11 raw per top-up
fn default_replenish_interval_secs() -> u64 {
    60
}

// -- Markets -------------------------------------------------------------

/// One underlying the bot makes markets in. Settlement is shared across all
/// markets (every bucket settles in the configured `settlement_symbol`), so
/// only the underlying-specific pricing context lives here.
struct Market {
    symbol: String,
    /// Underlying SPL mint (base58) — the key a bucket's `asset_mint` is
    /// matched against to pick this market. Byte-exact comparison.
    mint: String,
    mint_pubkey: Pubkey,
    feed: PriceFeedId,
    decimals: u8,
    /// Short-window realized-vol buffer fed from this underlying's USD price.
    vol_buf: Arc<RwLock<RollingVolBuffer>>,
    /// Long-window buffer (same samples); quoted sigma is max(short, long).
    vol_buf_long: Arc<RwLock<RollingVolBuffer>>,
    /// Sigma used while `vol_buf` is cold.
    fallback_vol: f64,
    /// Vol smile for this underlying.
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

/// Derive mode only: poll token-info and cleanly restart the process when a
/// new underlying is listed, so boot rebuilds the market set (the oracle
/// subscription and per-market tasks are fixed at boot, so a live add isn't
/// possible). Debounced — a new underlying must appear on two consecutive
/// polls before we restart, so a token-info blip never flaps the bot.
/// Removals and fetch failures never trigger a restart.
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
                // rebuilds the full market set + oracle subscription.
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
    let _obs = observability::init("solana-mm-bot");

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    observability::ops::spawn(cfg.health_addr);
    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    // Resolve the token catalog + program info from solana-token-info. Hard
    // cutover: if it's unreachable after the retry window we crash (no
    // solana-deployments.json fallback).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, Duration::from_secs(2))
        .await
        .with_context(|| {
            format!("fetching catalog from solana-token-info at {}", cli.token_info_url)
        })?;
    // The instruction builders use the program crates' declare_id!; if the
    // deployment registry disagrees, every tx targets the wrong program.
    if snapshot.core_program() != options_core::ID.to_string()
        || snapshot.venue_program() != auction_venue::ID.to_string()
    {
        tracing::warn!(
            registry_core = %snapshot.core_program(),
            compiled_core = %options_core::ID,
            registry_venue = %snapshot.venue_program(),
            compiled_venue = %auction_venue::ID,
            "solana-token-info program ids differ from the compiled program crates"
        );
    }

    // Underlying set: explicit allowlist, or — when empty — derived from
    // token-info's enabled catalog (with a watcher that restarts the bot to
    // pick up new listings).
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
        format!("settlement symbol {} not in solana-token-info catalog", cfg.settlement_symbol)
    })?;
    let settlement_feed = settlement_spec.pyth_feed().with_context(|| {
        format!("missing pythFeedId for settlement {}", cfg.settlement_symbol)
    })?;
    let settlement_decimals = settlement_spec.decimals;
    let settlement_mint = settlement_spec.mint.clone();
    let settlement_mint_pubkey = parse_pubkey(&settlement_mint, "settlement mint")?;

    // Build one Market per underlying. Vol buffers are created here; their
    // sampler tasks are spawned once the oracle subscription is up (below).
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
            mint: spec.mint.clone(),
            mint_pubkey: parse_pubkey(&spec.mint, "underlying mint")?,
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

    // Quote-signing key (ed25519 seed) from the secrets TOML.
    let quote_seed = bootstrap::parse_quote_seed(secrets.mm_quote_key()?)?;
    let quote_pubkey = solana_tx::quote::quote_pubkey(&quote_seed)?;
    let quote_signing_key = ed25519_dalek::SigningKey::from_bytes(&quote_seed);
    tracing::info!(scheme = "ed25519", "quote signer ready");

    // Chain bootstrap: MmAccount PDA, wallet ATAs, inventory floors.
    let wrap = SolanaClientWrapper::connect(&secrets, cfg.network)?;
    let wallet = wrap.signer.pubkey();
    let mm_account =
        bootstrap::ensure_mm_account(&wrap, cfg.mm_account_salt, &quote_pubkey).await?;
    tracing::info!(%mm_account, "mm account ready");

    let mut all_mints: Vec<Pubkey> = vec![settlement_mint_pubkey];
    all_mints.extend(markets.iter().map(|m| m.mint_pubkey));
    bootstrap::ensure_wallet_atas(&wrap, &all_mints).await?;

    // Inventory floors: settlement (Trader-MM pays premiums from the
    // MmAccount), then each underlying (Writer-MM writes collateral). The
    // faucet keeps the wallet funded on non-mainnet; without it we only
    // move what the wallet already holds.
    let http = reqwest::Client::new();
    let boot_inventory = [(
        settlement_mint_pubkey,
        cfg.settlement_symbol.clone(),
        cfg.bootstrap_settlement_amount,
    )]
    .into_iter()
    .chain(
        markets
            .iter()
            .map(|m| (m.mint_pubkey, m.symbol.clone(), cfg.bootstrap_underlying_amount)),
    );
    for (mint, symbol, floor) in boot_inventory {
        let params = bootstrap::InventoryParams {
            mm_account,
            mint,
            symbol: &symbol,
            floor,
            top_up: floor,
            faucet_url: cfg.faucet_url.as_deref(),
        };
        if let Err(e) = bootstrap::ensure_account_inventory(&wrap, &http, &params).await {
            tracing::warn!(symbol = %symbol, error = %format!("{e:#}"), "bootstrap inventory failed; continuing");
        }
    }

    // Keep each underlying's inventory topped up so the writer-MM (ask)
    // side never runs dry mid-test. One task per underlying. Only relevant
    // if we advertise writer_mm and auto-replenish is enabled.
    if cfg.roles.contains(&MmRole::WriterMm) && cfg.underlying_replenish_threshold > 0 {
        for m in &markets {
            bootstrap::spawn_replenish_task(bootstrap::ReplenishParams {
                secrets: secrets.clone(),
                network: cfg.network,
                mm_account,
                mint: m.mint_pubkey,
                symbol: m.symbol.clone(),
                floor: cfg.underlying_replenish_threshold,
                top_up: cfg.underlying_replenish_amount,
                faucet_url: cfg.faucet_url.clone(),
                interval_secs: cfg.underlying_replenish_interval_secs,
            });
        }
    }

    // Live prices come from solana-oracle-service (the single Pyth gateway)
    // over its WS fanout. `subscribe()` returns a PriceCache a background
    // task keeps current; the hot RFQ path reads it with the same
    // `get_fresh` staleness check as a direct SSE subscription.
    let oracle = oracle_client::OracleClient::new(&cli.oracle_url);
    let mut all_feeds: Vec<PriceFeedId> = markets.iter().map(|m| m.feed).collect();
    all_feeds.push(settlement_feed);
    let (price_cache, _ws_task) = oracle.subscribe();

    // Maintain each market's rolling-vol buffer from the live cache on the
    // configured cadence. The buffer warms from the stream within a few
    // samples and `fallback_vol` covers the brief cold-start window.
    for m in &markets {
        spawn_vol_sampler(
            cfg.pyth.clone(),
            m.symbol.clone(),
            m.feed,
            price_cache.clone(),
            vec![Arc::clone(&m.vol_buf), Arc::clone(&m.vol_buf_long)],
        );
    }

    // Derive mode: watch token-info for newly-listed underlyings and restart
    // to pick them up. No-op when underlyings were pinned explicitly.
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

    // Don't enter the RFQ loop until every feed (all underlyings +
    // settlement) has produced at least one observation. Otherwise early
    // RFQs decline for stale data.
    wait_for_first_prices(&price_cache, &all_feeds, Duration::from_secs(30)).await?;

    // RFQ pricing context — built once, reused across reconnects.
    let token_recipient = match &cfg.token_recipient {
        Some(s) => parse_pubkey(s, "token_recipient")?,
        None => wallet,
    };
    let protocol_id = parse_pubkey(snapshot.config_pda(), "config_pda")?;
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
    // solana-api-service client: the bot looks each RFQ's bucket up by
    // address to get its true (strike, expiry, mints, kind) rather than
    // trusting the broadcast.
    let api = ApiClient::new(&cli.api_url);
    tracing::info!(
        api_url = %cli.api_url,
        settlement = %cfg.settlement_symbol,
        "bucket lookups via solana-api-service"
    );
    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
        max_conf_bps: cfg.pyth.max_conf_bps,
    };

    // Unified on-chain auction bidders: one task per enabled mode, sharing
    // the pricing brain + vol buffers.
    for (enabled, mode) in [
        (cfg.onchain_auction.covered_call, BidderMode::CoveredCall),
        (cfg.onchain_auction.cash_secured_put, BidderMode::CashSecuredPut),
        (cfg.onchain_auction.swap, BidderMode::Swap),
    ] {
        if !enabled {
            continue;
        }
        let bidder_markets = markets
            .iter()
            .map(|m| auction::BidderMarket {
                symbol: m.symbol.clone(),
                mint: m.mint.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
                vol_buf_long: Arc::clone(&m.vol_buf_long),
                fallback_vol: m.fallback_vol,
                smile: m.smile,
            })
            .collect();
        auction::spawn_bidder(auction::BidderParams {
            mode,
            cfg: cfg.onchain_auction.clone(),
            secrets: secrets.clone(),
            network: cfg.network,
            indexer_graphql_url: cfg.indexer_graphql_url.clone(),
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: bidder_markets,
            settlement_feed,
            settlement_mint: settlement_mint.clone(),
            settlement_decimals,
            pricing: pricing_cfg,
            staleness,
        });
        tracing::info!(mode = mode.as_str(), "onchain auction bidder enabled");
    }

    // nonce is monotonic for the bot's lifetime — seeded from unix ms so a
    // restart never reuses a consumed nonce; kept across reconnects.
    let mut nonce_counter = now_ms();

    // Connect → authenticate → serve, reconnecting with capped exponential
    // backoff. A transient auth rejection — the indexer hasn't ingested our
    // AccountCreated yet (`auth_scheme_unknown`) — or a dropped connection
    // is expected right after a redeploy, so we keep the process (and its
    // /health endpoint) alive and retry until the indexer catches up. Only
    // a permanent auth error (a key mismatch the indexer will never accept)
    // is fatal.
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
                account_id: mm_account.to_string(),
                signing_scheme: options_core::state::SCHEME_ED25519,
                signing_pubkey: quote_pubkey.to_vec(),
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
        let sig = quote_signing_key.sign(&challenge).to_bytes().to_vec();
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
                    "auth permanently rejected — quote key does not match the registered account");
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
                ServiceToMm::AccountStateUpdate { .. } => "account_state_update",
                ServiceToMm::ReservationConfirmed { .. } => "reservation_confirmed",
                ServiceToMm::ReservationReleased { .. } => "reservation_released",
                _ => "other",
            };
            metrics::counter!("solana_mm_bot_ws_messages_total", "type" => frame_type)
                .increment(1);
            match frame {
                ServiceToMm::RFQBroadcast { request_id, payload } => {
                    tracing::debug!(
                        %request_id,
                        bucket_id = %payload.bucket_id,
                        write_amount = payload.write_amount,
                        "received rfq broadcast"
                    );
                    let rfq_start = std::time::Instant::now();
                    let now = now_ms();

                    // Resolve the bucket's true pricing inputs from
                    // solana-api-service by address. The broadcast carries no
                    // strike/expiry/pair, so a spoofed or buggy upstream can't
                    // trick us into mispricing.
                    let bucket = match api.bucket_pricing(&payload.bucket_id).await {
                        Ok(Some(b)) => b,
                        not_found_or_err => {
                            let reason = match not_found_or_err {
                                Ok(None) => "unknown or settled bucket".to_string(),
                                Err(e) => format!("bucket lookup failed: {e:#}"),
                                Ok(Some(_)) => unreachable!(),
                            };
                            metrics::counter!("solana_mm_bot_quote_failures_total", "reason" => "bucket_lookup")
                                .increment(1);
                            tracing::debug!(%request_id, %reason, "declining");
                            if let Err(e) = send_decline(&mut ws, &request_id, reason).await {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    // Pick the market whose pair this bucket belongs to. None
                    // means we don't source a spot for it — decline.
                    let market = match find_market(&markets, &bucket, &settlement_mint) {
                        Some(m) => m,
                        None => {
                            let reason = format!(
                                "pair not served: {}/{}",
                                bucket.asset_mint, bucket.settlement_mint
                            );
                            metrics::counter!("solana_mm_bot_quote_failures_total", "reason" => "pair_not_served")
                                .increment(1);
                            tracing::debug!(%request_id, %reason, "declining");
                            if let Err(e) = send_decline(&mut ws, &request_id, reason).await {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                            continue 'serve;
                        }
                    };

                    // Live spot for this market, scaled into the bucket's
                    // units (settlement smallest-units per underlying
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
                            metrics::counter!("solana_mm_bot_quote_failures_total", "reason" => "stale_price")
                                .increment(1);
                            tracing::debug!(%request_id, reason, "declining: stale market data");
                            if let Err(e) = send_decline(
                                &mut ws,
                                &request_id,
                                format!("stale market data: {reason}"),
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
                            let bucket_pubkey =
                                match parse_pubkey(&payload.bucket_id, "bucket_id") {
                                    Ok(b) => b,
                                    Err(e) => {
                                        tracing::warn!(error = %e, "unparseable bucket id on broadcast");
                                        continue 'serve;
                                    }
                                };
                            nonce_counter = nonce_counter.wrapping_add(1);
                            let quote = Quote {
                                protocol_id,
                                signer_account: mm_account,
                                signer_token_recipient: token_recipient,
                                bucket: bucket_pubkey,
                                write_amount: payload.write_amount,
                                premium,
                                valid_until_ms,
                                nonce: nonce_counter,
                            };
                            // Detached ed25519 over the canonical Borsh bytes —
                            // exactly what the Ed25519SigVerify precompile the
                            // executor builds will verify.
                            let sig = sign_quote(&quote_seed, &quote)?;
                            if let Err(e) = ws_client::send_json(
                                &mut ws,
                                &MmToService::Quote {
                                    request_id,
                                    payload: MmQuotePayload {
                                        quote: QuoteWire::from(&quote),
                                        signature: sig.to_vec(),
                                    },
                                },
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "ws send (quote) failed; reconnecting");
                                break 'serve;
                            }
                            metrics::counter!("solana_mm_bot_quotes_signed_total").increment(1);
                            metrics::histogram!("solana_mm_bot_rfq_response_duration_seconds")
                                .record(rfq_start.elapsed().as_secs_f64());
                            tracing::info!(premium, nonce = nonce_counter, "quote sent");
                        }
                        PriceDecision::Decline { reason } => {
                            metrics::counter!("solana_mm_bot_quote_failures_total", "reason" => "price_declined")
                                .increment(1);
                            tracing::debug!(%request_id, %reason, "declining");
                            if let Err(e) = send_decline(&mut ws, &request_id, reason).await {
                                tracing::warn!(error = %e, "ws send (decline) failed; reconnecting");
                                break 'serve;
                            }
                        }
                    }
                }
                ServiceToMm::BulkViewRFQBroadcast { request_id, payload } => {
                    tracing::debug!(
                        %request_id,
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
                        // Resolve each bucket from solana-api-service (cached);
                        // skip ones we can't price — a bulk-view bucket has no
                        // per-bucket decline.
                        let bucket = match api.bucket_pricing(bucket_id).await {
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
                                    &bucket.asset_mint,
                                    &bucket.settlement_mint,
                                    &m.mint,
                                    &settlement_mint,
                                )
                            })
                            .and_then(|i| spots[i])
                        else {
                            continue;
                        };
                        // Reuse the signed-RFQ pricer; we keep only the premium
                        // — no Quote is built, no nonce burned, nothing signed.
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
                                bucket_id: bucket_id.clone(),
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
                ServiceToMm::AccountStateUpdate { .. } => {
                    tracing::trace!("received account state update");
                }
                ServiceToMm::ReservationConfirmed { .. } => {
                    tracing::trace!("received reservation confirmed");
                }
                ServiceToMm::ReservationReleased { .. } => {
                    tracing::trace!("received reservation released");
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
        roles = ?cfg.roles,
        quote_ttl_ms = cfg.quote_ttl_ms,
        rate = cfg.rate,
        "bot config loaded"
    );
    Ok(cfg)
}

fn parse_pubkey(s: &str, what: &str) -> Result<Pubkey> {
    s.parse::<Pubkey>()
        .map_err(|e| anyhow!("{what} is not a base58 pubkey ({s}): {e}"))
}

async fn send_decline(
    ws: &mut ws_client::WsStream,
    request_id: &str,
    reason: String,
) -> Result<()> {
    ws_client::send_json(
        ws,
        &MmToService::Decline {
            request_id: request_id.to_string(),
            payload: DeclinePayload { reason },
        },
    )
    .await
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
    /// AccountCreated yet). Retry until it catches up.
    Retryable { code: String, message: String },
    /// A permanent rejection (`auth_pubkey_mismatch` /
    /// `auth_signature_invalid`): the registered key will never match what
    /// we present. Fatal — retrying can't fix a misconfigured key.
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
                    "oracle: no observation within {:?} for feed {missing}",
                    timeout
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            tracing::info!(feeds = feeds.len(), "oracle: first prices observed for all feeds");
            return Ok(());
        }
    }
}

/// Pick the market whose pair matches this bucket. `None` when no configured
/// market quotes the bucket's `(underlying, settlement)` pair.
fn find_market<'a>(
    markets: &'a [Market],
    bucket: &solana_mm_bot::api_client::BucketPricing,
    settlement_mint: &str,
) -> Option<&'a Market> {
    markets.iter().find(|m| {
        serves_pair(
            &bucket.asset_mint,
            &bucket.settlement_mint,
            &m.mint,
            settlement_mint,
        )
    })
}

/// Maintain one market's vol buffers from the live price cache on the
/// configured cadence. The buffer warms from the live stream;
/// `fallback_vol` covers the cold-start window.
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
            metrics::gauge!("solana_mm_bot_pyth_price_age_seconds", "symbol" => symbol.clone())
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
