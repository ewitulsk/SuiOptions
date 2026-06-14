//! Market-maker bot.
//!
//! Phase 1: bootstrap.
//!   - Reads its TOML config (incl. `signing_scheme`) + `MM_QUOTE_KEY`
//!     (32-byte hex secret — interpretation depends on the scheme).
//!   - Resolves the test-token pair (`underlying_symbol` / `settlement_symbol`)
//!     against `deployments.json`.
//!   - Resolves its Account from chain state for the *current* deployment:
//!     looks up the `AccountCreated` event under the current package for this
//!     bot's Sui address. If none exists (e.g. right after a fresh contract
//!     deployment), calls `account::create_and_share_account(scheme, pubkey)`
//!     and funds it with `bootstrap_settlement_amount` of the settlement
//!     asset via `test_tokens::<sym>::mint` + `account::deposit`. No local
//!     state is persisted — the deployment is the source of truth.
//!
//! Phase 2: serve.
//!   - Authenticates over WS via the scheme-aware challenge (§5.4.1).
//!   - Loops on `RFQBroadcast`, prices each option via Black-Scholes using
//!     the spot/vol/rate config, signs the BCS-encoded Quote with the
//!     configured scheme, sends. Pongs Pings.
//!
//! The MM serves as a **Trader MM** by default (pays premium, receives the
//! call token). `roles` in the TOML controls advertised roles to the
//! quoting service.

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

use pyth_client::{self as pyth, PriceCache, PriceFeedId, RollingVolBuffer};
use api_service_client::ApiServiceClient;
use token_info_client::TokenInfoClient;
use sui_tx::quote_signer::QuoteSigner;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::account::{account_balance_of, create_and_share_account, find_account};
use sui_tx::tx::test_tokens::mint_and_deposit_into_account;
use sui_tx::ws_client;

use mm_bot::liquidity::{FaucetLiquiditySource, LiquiditySource};
use mm_bot::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, serves_pair, PriceDecision, PricingConfig,
    RfqPricingInputs, Staleness,
};
use mm_bot::Cli;

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

    /// Underlyings to make markets in. Each symbol is looked up in the
    /// token-info catalog (coin type, decimals, `pythFeedId`) and quoted
    /// against the shared `settlement_symbol`. The bot subscribes to every
    /// underlying's Pyth feed, keeps a per-underlying vol buffer, and prices
    /// each bucket against its own pair's spot. A bucket whose pair isn't in
    /// this list is declined.
    #[serde(default = "default_underlying_symbols")]
    underlying_symbols: Vec<String>,
    #[serde(default = "default_settlement")]
    settlement_symbol: String,

    /// Annualized risk-free rate. Pyth doesn't price the curve; this
    /// stays a config knob.
    #[serde(default)]
    rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,

    /// Ask-side markup in basis points, applied when quoting as the Writer MM
    /// (retail buying — trader flow): premium is marked *up* off the
    /// Black-Scholes mid. Defaults to 100 (1%).
    #[serde(default = "default_spread_bps")]
    ask_markup_bps: u64,
    /// Bid-side markdown in basis points, applied when quoting as the Trader
    /// MM (retail writing — writer flow): premium is marked *down* off the
    /// mid. Defaults to 100 (1%).
    #[serde(default = "default_spread_bps")]
    bid_markdown_bps: u64,

    /// Roles advertised to the quoting service.
    roles: Vec<MmRole>,

    /// Opt in to answering unsigned bulk-view RFQs (indicative premiums for
    /// the frontend's tiles). These are priced but never signed — no nonce is
    /// consumed and nothing reaches the chain. Defaults to false.
    #[serde(default)]
    bulk_view_enabled: bool,

    /// Where minted call tokens / position NFTs should land. Defaults to
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

    /// On-chain RFQ bidder (doc 05 Â§3) â the buy side of the vault's
    /// weekly call-slice auctions. Off by default.
    #[serde(default)]
    onchain_rfq: mm_bot::onchain_rfq::OnchainRfqConfig,

    /// On-chain proceeds-swap bidder (doc 05 §3.1) — the buy side of the
    /// vault's settlement→underlying swap auctions. Off by default.
    #[serde(default)]
    onchain_swap: mm_bot::onchain_swap::OnchainSwapConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct PythConfig {
    /// Hermes base URL. Mainnet default; override for a private mirror.
    hermes_url: String,
    /// Benchmarks base URL. Used for the realized-vol cold-start sample.
    benchmarks_url: String,
    /// Reject an RFQ if our last *observation* of either price is older
    /// than this. Catches a wedged or disconnected stream.
    max_price_age_ms: u64,
    /// Reject an RFQ if Pyth's publisher timestamp is older than this.
    /// Catches the case where the stream is alive but Pyth itself isn't
    /// publishing.
    max_publish_lag_ms: u64,
    /// Rolling window (in hours) used to compute realized vol.
    vol_window_hours: u64,
    /// How often the live SSE feed is sampled into the vol buffer. The
    /// vol estimate annualizes from this cadence.
    vol_sample_interval_ms: u64,
    /// Number of historical points to fetch from Benchmarks at startup
    /// to seed the vol buffer.
    bootstrap_samples: u32,
    /// Seconds between adjacent bootstrap samples.
    bootstrap_interval_secs: u64,
    /// Volatility used until the buffer has enough samples. Once it does,
    /// the live estimate takes over.
    fallback_vol: f64,
}

impl Default for PythConfig {
    fn default() -> Self {
        Self {
            hermes_url: "https://hermes.pyth.network".into(),
            benchmarks_url: "https://benchmarks.pyth.network".into(),
            max_price_age_ms: 5_000,
            max_publish_lag_ms: 10_000,
            vol_window_hours: 24,
            vol_sample_interval_ms: 60_000,
            bootstrap_samples: 24,
            bootstrap_interval_secs: 3_600,
            fallback_vol: 0.6,
        }
    }
}

fn default_scheme() -> SigningScheme {
    SigningScheme::Ed25519
}

fn default_underlying_symbols() -> Vec<String> {
    vec!["TBTC".into()]
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
} // 1% markup/markdown off the BS mid
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
    /// Realized-vol buffer fed from this underlying's USD price.
    vol_buf: Arc<RwLock<RollingVolBuffer>>,
}

// -- Main loop -----------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _obs = observability::init("mm-bot");

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    observability::ops::spawn(cfg.health_addr);
    let secrets_loaded = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
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
    if cfg.underlying_symbols.is_empty() {
        anyhow::bail!("no underlying_symbols configured — nothing to make markets in");
    }
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
    let samples_per_year =
        (365.0 * 24.0 * 60.0 * 60.0 * 1000.0) / cfg.pyth.vol_sample_interval_ms as f64;
    let vol_window_ms = cfg.pyth.vol_window_hours.saturating_mul(3_600_000);
    let mut markets: Vec<Market> = Vec::with_capacity(cfg.underlying_symbols.len());
    for sym in &cfg.underlying_symbols {
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
            vol_buf: Arc::new(RwLock::new(RollingVolBuffer::new(
                vol_window_ms,
                samples_per_year,
            ))),
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

    // Account: bootstrap if missing, then fund with settlement.
    let account_id =
        resolve_account(&cli, &cfg, &snapshot, &secrets_loaded, &signer, &pubkey_bytes).await?;
    tracing::info!(account_id = %account_id, "mm account ready");
    let account_id_pt = pt_object_id_from_sui(account_id);

    // Liquidity source: pulls settlement (and, via the same trait, any coin the
    // bot needs) before quoting. Default = the test-token faucet; a real market
    // maker swaps in their own funding source at this one site.
    let liquidity: Arc<dyn LiquiditySource> = Arc::new(FaucetLiquiditySource::new(
        snapshot.maybe_test_tokens(),
        snapshot.package()?,
        cli.gas_budget,
    ));

    // Keep each underlying's inventory topped up so the writer-MM (ask) side
    // never runs dry mid-test. One task per underlying. Only relevant if we
    // advertise writer_mm and auto-replenish is enabled.
    if cfg.roles.contains(&MmRole::WriterMm) && cfg.underlying_replenish_threshold > 0 {
        let package = snapshot.package()?;
        for sym in &cfg.underlying_symbols {
            let underlying = snapshot.faucet_token(sym)?;
            spawn_replenish_task(ReplenishParams {
                secrets: secrets_loaded.clone(),
                network: cfg.network,
                package,
                account_id,
                coin_type: underlying.coin_type.clone(),
                symbol: sym.clone(),
                threshold: cfg.underlying_replenish_threshold,
                top_up: cfg.underlying_replenish_amount,
                interval_secs: cfg.underlying_replenish_interval_secs,
                liquidity: Arc::clone(&liquidity),
            });
        }
    }

    // Pyth client + live price cache + rolling-vol buffer. The SSE task
    // owns a tokio task that pushes into the cache; the bootstrap +
    // sampler task seeds and maintains the vol buffer.
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;
    let price_cache = PriceCache::new();
    // Subscribe to every underlying's feed plus the shared settlement feed.
    let mut all_feeds: Vec<PriceFeedId> = markets.iter().map(|m| m.feed).collect();
    all_feeds.push(settlement_feed);
    let stream_rx = pyth::spawn_subscriber(
        http_client.clone(),
        cfg.pyth.hermes_url.clone(),
        all_feeds.clone(),
    );
    price_cache.spawn_updater(stream_rx);

    // One vol sampler per underlying — vol is keyed off each underlying's own
    // USD price. Annualization factor follows the sample cadence.
    for m in &markets {
        spawn_vol_task(
            http_client.clone(),
            cfg.pyth.clone(),
            m.symbol.clone(),
            m.feed,
            price_cache.clone(),
            Arc::clone(&m.vol_buf),
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
    };

    // DeepBook quoting loop (SO-158): rest two-sided limit orders on every
    // tradeable bucket pool of the configured markets, priced by the same
    // Black-Scholes path that answers RFQs (one QuoterMarket per Market,
    // sharing its vol buffer — SO-159).
    if cfg.deepbook.enabled {
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
                    fallback_vol: cfg.pyth.fallback_vol,
                    liquidity: Arc::clone(&liquidity),
                });
                tracing::info!(markets = cfg.underlying_symbols.len(), "deepbook quoting enabled");
            }
            None => tracing::warn!(
                "deepbook.enabled set but token-info reports no DeepBook deployment; quoting disabled"
            ),
        }
    }

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
            })
            .collect();
        mm_bot::onchain_rfq::spawn_bidder(mm_bot::onchain_rfq::BidderParams {
            cfg: cfg.onchain_rfq.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            package: snapshot.package()?,
            api_url: cli.api_url.clone(),
            price_cache: price_cache.clone(),
            markets: bidder_markets,
            settlement_feed,
            settlement_coin_type: settlement_coin_type.clone(),
            settlement_decimals,
            pricing: pricing_cfg,
            staleness,
            fallback_vol: cfg.pyth.fallback_vol,
        });
        tracing::info!("onchain rfq bidder enabled");
    }

    // On-chain swap bidder: the buy side of the vault's proceeds-swap
    // auctions (settlement → underlying), discovered straight from
    // SwapRfqCreated events.
    if cfg.onchain_swap.bidder.enabled {
        let swap_markets = markets
            .iter()
            .map(|m| mm_bot::onchain_rfq::BidderMarket {
                symbol: m.symbol.clone(),
                coin_type: m.coin_type.clone(),
                feed: m.feed,
                decimals: m.decimals,
                vol_buf: Arc::clone(&m.vol_buf),
            })
            .collect();
        mm_bot::onchain_swap::spawn_bidder(mm_bot::onchain_swap::SwapBidderParams {
            cfg: cfg.onchain_swap.clone(),
            secrets: secrets_loaded.clone(),
            network: cfg.network,
            package: snapshot.package()?,
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
    // AccountCreated yet (`auth_scheme_unknown`) — or a dropped connection is
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
                account_id: account_id_pt,
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
                ServiceToMm::AccountStateUpdate { .. } => "account_state_update",
                ServiceToMm::ReservationConfirmed { .. } => "reservation_confirmed",
                ServiceToMm::ReservationReleased { .. } => "reservation_released",
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
                        cfg.pyth.fallback_vol,
                    );

                    let inputs = RfqPricingInputs {
                        write_amount: payload.write_amount,
                        side: payload.side,
                        strike: bucket.strike,
                        strike_scale: bucket.strike_scale,
                        expiry_ms: bucket.expiry_ms,
                    };
                    match price_rfq(&pricing_cfg, &inputs, spot_scaled, sigma, now) {
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
                                signer_account_id: account_id_pt,
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
                    let spots: Vec<Option<(f64, f64)>> = markets
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
                                        cfg.pyth.fallback_vol,
                                    ),
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
                        let Some((spot_scaled, sigma)) = markets
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
                        };
                        if let PriceDecision::Quote { premium, .. } =
                            price_rfq(&pricing_cfg, &inputs, spot_scaled, sigma, now)
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

async fn resolve_account(
    cli: &Cli,
    cfg: &BotConfig,
    snapshot: &token_info_client::Snapshot,
    secrets: &runtime_config::Secrets,
    signer: &QuoteSigner,
    pubkey_bytes: &[u8],
) -> Result<ObjectID> {
    let wrap = SuiClientWrapper::connect(secrets, cfg.network).await?;
    let package = snapshot.package()?;

    // The deployment is the source of truth — no local sidecar. If this
    // bot's Sui address already created an Account under the current package,
    // adopt it; otherwise bootstrap a fresh one. A fresh contract deployment
    // (new package) has no such event, so the bot self-heals by creating a
    // new account against the package the indexer is actually watching.
    if let Some(account_id) =
        find_account(&wrap.client, package, wrap.signer.address, signer.scheme(), pubkey_bytes)
            .await?
    {
        tracing::info!(%account_id, "adopted existing on-chain account for this deployment");
        return Ok(account_id);
    }

    tracing::info!("no account for the current deployment — bootstrapping a fresh Account");
    let created = create_and_share_account(
        &wrap.client,
        &wrap.signer,
        package,
        signer.scheme(),
        pubkey_bytes,
        cli.gas_budget,
    )
    .await?;
    tracing::info!(digest = %created.digest, account_id = %created.account_id, "account created");

    // Fund it with settlement so it can pay premiums on day one (Trader-MM /
    // bid side). Create and fund are separate txs; a crash between them leaves
    // the account (adopted on the next boot) unfunded — acceptable for the
    // test MM bot.
    let settlement = snapshot.faucet_token(&cfg.settlement_symbol)?;
    let (tokens_pkg, settlement_module) = settlement.module_path()?;
    let fund_resp = mint_and_deposit_into_account(
        &wrap.client,
        &wrap.signer,
        tokens_pkg,
        &settlement_module,
        settlement.faucet()?,
        &settlement.coin_type,
        created.account_id,
        package,
        cfg.bootstrap_settlement_amount,
        cli.gas_budget,
    )
    .await?;
    tracing::info!(
        digest = %fund_resp.digest,
        amount = cfg.bootstrap_settlement_amount,
        symbol = %cfg.settlement_symbol,
        "account funded (settlement)"
    );

    // Fund it with each underlying so it can write calls to retail traders
    // (Writer-MM / ask side). The background replenish tasks keep these topped
    // up as the inventory drains.
    for sym in &cfg.underlying_symbols {
        let underlying = snapshot.faucet_token(sym)?;
        let (u_tokens_pkg, underlying_module) = underlying.module_path()?;
        let fund_resp = mint_and_deposit_into_account(
            &wrap.client,
            &wrap.signer,
            u_tokens_pkg,
            &underlying_module,
            underlying.faucet()?,
            &underlying.coin_type,
            created.account_id,
            package,
            cfg.bootstrap_underlying_amount,
            cli.gas_budget,
        )
        .await?;
        tracing::info!(
            digest = %fund_resp.digest,
            amount = cfg.bootstrap_underlying_amount,
            symbol = %sym,
            "account funded (underlying)"
        );
    }

    Ok(created.account_id)
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
    package: ObjectID,
    account_id: ObjectID,
    coin_type: String,
    symbol: String,
    threshold: u64,
    top_up: u64,
    interval_secs: u64,
    /// Source the top-up is pulled from (faucet by default). The faucet id /
    /// module / gas are resolved inside the source from `coin_type`.
    liquidity: Arc<dyn LiquiditySource>,
}

/// Periodically read the Account's underlying balance (via devInspect, no gas)
/// and mint+deposit a top-up when it drops below the configured threshold.
/// Runs in its own tokio task with its own Sui client so it doesn't contend
/// with the WS serve loop. Transient errors are logged and retried on the next
/// tick — a wedged faucet shouldn't kill the bot.
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
            let balance = match account_balance_of(
                &wrap.client,
                wrap.signer.address,
                p.package,
                p.account_id,
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
                    p.account_id,
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

/// Bootstrap the vol buffer from Pyth Benchmarks (one historical sample
/// per hour by default), then maintain it by sampling the live cache on
/// the configured cadence. The whole thing lives in a single tokio task.
fn spawn_vol_task(
    client: reqwest::Client,
    cfg: PythConfig,
    symbol: String,
    underlying_feed: PriceFeedId,
    cache: PriceCache,
    buf: Arc<RwLock<RollingVolBuffer>>,
) {
    tokio::spawn(async move {
        // --- bootstrap from Benchmarks --------------------------------------
        // Walk back N points spaced by `bootstrap_interval_secs`. Pace at one
        // call per second so we stay under the 10-req/10s ceiling.
        let now_secs = (now_ms() / 1000) as i64;
        for i in (0..cfg.bootstrap_samples).rev() {
            let ts = now_secs - (i as i64) * cfg.bootstrap_interval_secs as i64;
            match pyth::benchmark_at(&client, &cfg.benchmarks_url, underlying_feed, ts).await {
                Ok(upd) => match upd.price.price_f64() {
                    Ok(p) => {
                        let ts_ms = (ts as u64).saturating_mul(1000);
                        buf.write().push(ts_ms, p);
                        tracing::debug!(ts, price = p, "vol bootstrap sample");
                    }
                    Err(e) => tracing::debug!(error = %e, "vol bootstrap parse failed"),
                },
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), ts, "vol bootstrap fetch failed");
                }
            }
            tokio::time::sleep(Duration::from_millis(1_100)).await;
        }
        if let Some(sigma) = buf.read().current_annualized() {
            tracing::info!(sigma, "vol buffer bootstrapped");
        } else {
            tracing::warn!("vol bootstrap produced too few samples; using fallback until live data fills the window");
        }

        // --- maintain from the live cache -----------------------------------
        let mut ticker = tokio::time::interval(Duration::from_millis(cfg.vol_sample_interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut vol_log_counter: u64 = 0;
        loop {
            ticker.tick().await;
            let Some(cp) = cache.peek(underlying_feed) else {
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
            buf.write().push(now_ms(), cp.price);
            if let Some(sigma) = buf.read().current_annualized() {
                vol_log_counter += 1;
                if vol_log_counter % 60 == 1 {
                    tracing::debug!(sigma, samples = buf.read().len(), "vol updated");
                }
            }
        }
    });
}
