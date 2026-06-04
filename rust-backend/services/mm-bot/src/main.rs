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
    AuthResponsePayload, MmHelloPayload, MmQuotePayload, MmToService, ServiceToMm,
};
use protocol_types::quote::Quote;
use protocol_types::sides::MmRole;
use protocol_types::SigningScheme;

use pyth_client::{self as pyth, PriceCache, PriceFeedId, RollingVolBuffer};
use token_info_client::TokenInfoClient;
use sui_tx::quote_signer::QuoteSigner;
use sui_tx::sui_client::{Network, SuiClientWrapper};
use sui_tx::tx::account::{create_and_share_account, find_account};
use sui_tx::tx::test_tokens::mint_and_deposit_into_account;
use sui_tx::ws_client;

use mm_bot::pricing::{
    compute_spot_from_cache, price_rfq, resolve_sigma, PriceDecision, PricingConfig, Staleness,
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

    /// Asset pair to quote on. Symbols are looked up in
    /// `deployments.json::testTokens.tokens`, and each must carry a
    /// `pythFeedId` so the bot can source live prices.
    #[serde(default = "default_underlying")]
    underlying_symbol: String,
    #[serde(default = "default_settlement")]
    settlement_symbol: String,

    /// Annualized risk-free rate. Pyth doesn't price the curve; this
    /// stays a config knob.
    #[serde(default)]
    rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,

    /// Roles advertised to the quoting service.
    roles: Vec<MmRole>,

    /// Where minted call tokens / position NFTs should land. Defaults to
    /// the bot's Sui address.
    #[serde(default)]
    token_recipient: Option<String>,

    /// On first run, mint+deposit this much settlement asset into the
    /// freshly-created Account so it can pay premiums.
    #[serde(default = "default_bootstrap_amount")]
    bootstrap_settlement_amount: u64,

    /// Pyth Hermes/Benchmarks settings. All fields have defaults.
    #[serde(default)]
    pyth: PythConfig,
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

fn default_underlying() -> String {
    "TBTC".into()
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

// -- Main loop -----------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    runtime_config::logging::init();

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    runtime_config::health::spawn(cfg.health_addr);
    let secrets_loaded = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    // Resolve the token catalog from token-info. Hard cutover: if token-info
    // is unreachable after the retry window we crash (no deployments.json
    // fallback).
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await
        .with_context(|| format!("fetching catalog from token-info at {}", cli.token_info_url))?;

    // Off-chain token catalog lookup (coin type, decimals, pyth feed).
    // This is the source the pricing path reads from; the bootstrap path
    // separately looks up the test-token faucet via `snapshot.token(symbol)`.
    let underlying_spec = snapshot.token_spec(&cfg.underlying_symbol).with_context(|| {
        format!(
            "underlying symbol {} not in token-info catalog",
            cfg.underlying_symbol
        )
    })?;
    let settlement_spec = snapshot.token_spec(&cfg.settlement_symbol).with_context(|| {
        format!(
            "settlement symbol {} not in token-info catalog",
            cfg.settlement_symbol
        )
    })?;

    let underlying_feed = underlying_spec.pyth_feed().with_context(|| {
        format!("missing pythFeedId for underlying {}", cfg.underlying_symbol)
    })?;
    let settlement_feed = settlement_spec.pyth_feed().with_context(|| {
        format!("missing pythFeedId for settlement {}", cfg.settlement_symbol)
    })?;
    let underlying_decimals = underlying_spec.decimals;
    let settlement_decimals = settlement_spec.decimals;
    tracing::info!(
        underlying = %cfg.underlying_symbol,
        underlying_feed = %underlying_feed,
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

    // Pyth client + live price cache + rolling-vol buffer. The SSE task
    // owns a tokio task that pushes into the cache; the bootstrap +
    // sampler task seeds and maintains the vol buffer.
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;
    let price_cache = PriceCache::new();
    let stream_rx = pyth::spawn_subscriber(
        http_client.clone(),
        cfg.pyth.hermes_url.clone(),
        vec![underlying_feed, settlement_feed],
    );
    price_cache.spawn_updater(stream_rx);

    // Vol buffer is keyed off the underlying's USD price. Annualization
    // factor follows the sample cadence.
    let samples_per_year =
        (365.0 * 24.0 * 60.0 * 60.0 * 1000.0) / cfg.pyth.vol_sample_interval_ms as f64;
    let vol_buf = Arc::new(RwLock::new(RollingVolBuffer::new(
        cfg.pyth.vol_window_hours.saturating_mul(3_600_000),
        samples_per_year,
    )));
    spawn_vol_task(
        http_client.clone(),
        cfg.pyth.clone(),
        underlying_feed,
        price_cache.clone(),
        Arc::clone(&vol_buf),
    );

    // Don't enter the RFQ loop until both feeds have produced at least one
    // observation. Otherwise every early RFQ gets declined for stale data.
    wait_for_first_prices(
        &price_cache,
        underlying_feed,
        settlement_feed,
        Duration::from_secs(30),
    )
    .await?;

    // RFQ pricing context — built once, reused across reconnects.
    let token_recipient = resolve_token_recipient(&cfg, &secrets_loaded)?;
    let protocol_id = snapshot.protocol_id_bytes()?;
    let pricing_cfg = PricingConfig {
        rate: cfg.rate,
        quote_ttl_ms: cfg.quote_ttl_ms,
    };
    let staleness = Staleness {
        max_price_age: Duration::from_millis(cfg.pyth.max_price_age_ms),
        max_publish_lag: Duration::from_millis(cfg.pyth.max_publish_lag_ms),
    };
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
            match frame {
                ServiceToMm::RFQBroadcast {
                    request_id,
                    payload,
                } => {
                    tracing::debug!(
                        ?request_id,
                        strike = %payload.strike,
                        strike_scale = payload.strike_scale,
                        expiry_ms = payload.expiry_ms,
                        write_amount = payload.write_amount,
                        "received rfq broadcast"
                    );
                    let now = now_ms();

                    // Live spot from Pyth, scaled into the bucket's units
                    // (settlement smallest-units per underlying smallest-unit).
                    let spot_scaled = match compute_spot_from_cache(
                        &price_cache,
                        underlying_feed,
                        settlement_feed,
                        underlying_decimals,
                        settlement_decimals,
                        staleness,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            let reason: &'static str = e.as_str();
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
                    let sigma =
                        resolve_sigma(vol_buf.read().current_annualized(), cfg.pyth.fallback_vol);

                    match price_rfq(&pricing_cfg, &payload, spot_scaled, sigma, now) {
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
                                spot = spot_scaled,
                                sigma,
                                strike = strike_scaled,
                                strike_raw = %payload.strike,
                                strike_scale = payload.strike_scale,
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
                            tracing::info!(premium, nonce = nonce_counter, "quote sent");
                        }
                        PriceDecision::Decline { reason } => {
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
        underlying = %cfg.underlying_symbol,
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

    // Fund it with settlement so it can pay premiums on day one. Create and
    // fund are separate txs; a crash between them leaves the account
    // (adopted on the next boot) unfunded — acceptable for the test MM bot.
    let settlement = snapshot.token(&cfg.settlement_symbol)?;
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
        "account funded"
    );

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

/// Block until both feeds have at least one cached observation (ignoring
/// the publish-lag bound). Times out so a misconfigured feed id surfaces
/// quickly rather than hanging the bot forever.
async fn wait_for_first_prices(
    cache: &PriceCache,
    a: PriceFeedId,
    b: PriceFeedId,
    timeout: Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        if cache.peek(a).is_some() && cache.peek(b).is_some() {
            tracing::info!("pyth: first prices observed for both feeds");
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "pyth: no observation within {:?} for one of {a} / {b}",
                timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Bootstrap the vol buffer from Pyth Benchmarks (one historical sample
/// per hour by default), then maintain it by sampling the live cache on
/// the configured cadence. The whole thing lives in a single tokio task.
fn spawn_vol_task(
    client: reqwest::Client,
    cfg: PythConfig,
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
