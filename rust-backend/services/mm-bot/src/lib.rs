//! Library surface for the `mm-bot` binary.
//!
//! The bot's full lifecycle (config load, account bootstrap, Pyth wiring,
//! quote loop) lives in [`run`] so tests in `integration-tests` can drive
//! the real bot in-process. `main.rs` is a thin clap shim that calls
//! `run`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;

use shared::protocol_types::ids::{ObjectId as PtObjectId, SuiAddress as PtSuiAddress};
use shared::protocol_types::messages::{
    AuthResponsePayload, MmHelloPayload, MmQuotePayload, MmToService, ServiceToMm,
};
use shared::protocol_types::quote::Quote;
use shared::protocol_types::sides::MmRole;
use shared::protocol_types::SigningScheme;

use shared::deployments::Deployments;
use shared::pricing::{call_price_per_unit, premium_for_write, CallInputs};
use shared::pyth::{self, PriceCache, PriceFeedId, RollingVolBuffer};
use shared::quote_signer::QuoteSigner;
use shared::sui_client::{Network, SuiClientWrapper};
use shared::tx::account::create_and_share_account;
use shared::tx::test_tokens::mint_and_deposit_into_account;
use shared::ws_client;

#[derive(Parser, Debug)]
#[command(name = "mm-bot", about = "Test market-maker bot for the options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "services/mm-bot/config/config.toml")]
    pub config: PathBuf,

    #[arg(long, default_value = "services/mm-bot/config/mm-bot.account.json")]
    pub account_state: PathBuf,

    #[arg(short, long, default_value = "deployments.json")]
    pub deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key (`sui.testnet`)
    /// and the quote-signing key (`mm_bot.quote_key`). No env-var fallback.
    #[arg(short = 's', long, default_value = "services/mm-bot/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,
}

shared::define_program! {
    id          = "mm-bot",
    cargo_pkg   = "mm-bot",
    working_dir = ".",
    description = "Market-maker bot. First run bootstraps a shared Account and funds it with \
                   settlement via the faucet; every run authenticates over WS and prices \
                   incoming RFQs with Black-Scholes.",
    cli         = crate::Cli,
}

// -- Config --------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub quoting_url: String,

    /// Quote-signing scheme. Stored on chain alongside the pubkey; the
    /// `MM_QUOTE_KEY` env var holds the 32-byte secret in this scheme.
    /// One of `ed25519` / `secp256k1` / `secp256r1`.
    #[serde(default = "default_scheme")]
    pub signing_scheme: SigningScheme,

    /// Asset pair to quote on. Symbols are looked up in
    /// `deployments.json::testTokens.tokens`, and each must carry a
    /// `pythFeedId` so the bot can source live prices.
    #[serde(default = "default_underlying")]
    pub underlying_symbol: String,
    #[serde(default = "default_settlement")]
    pub settlement_symbol: String,

    /// Annualized risk-free rate. Pyth doesn't price the curve; this
    /// stays a config knob.
    #[serde(default)]
    pub rate: f64,
    #[serde(default = "default_quote_ttl_ms")]
    pub quote_ttl_ms: u64,

    /// Roles advertised to the quoting service.
    pub roles: Vec<MmRole>,

    /// Where minted call tokens / position NFTs should land. Defaults to
    /// the bot's Sui address.
    #[serde(default)]
    pub token_recipient: Option<String>,

    /// On first run, mint+deposit this much settlement asset into the
    /// freshly-created Account so it can pay premiums.
    #[serde(default = "default_bootstrap_amount")]
    pub bootstrap_settlement_amount: u64,

    /// Pyth Hermes/Benchmarks settings. All fields have defaults.
    #[serde(default)]
    pub pyth: PythConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PythConfig {
    /// Hermes base URL. Mainnet default; override for a private mirror.
    pub hermes_url: String,
    /// Benchmarks base URL. Used for the realized-vol cold-start sample.
    pub benchmarks_url: String,
    /// Reject an RFQ if our last *observation* of either price is older
    /// than this.
    pub max_price_age_ms: u64,
    /// Reject an RFQ if Pyth's publisher timestamp is older than this.
    pub max_publish_lag_ms: u64,
    /// Rolling window (in hours) used to compute realized vol.
    pub vol_window_hours: u64,
    /// How often the live SSE feed is sampled into the vol buffer.
    pub vol_sample_interval_ms: u64,
    /// Number of historical points to fetch from Benchmarks at startup.
    pub bootstrap_samples: u32,
    /// Seconds between adjacent bootstrap samples.
    pub bootstrap_interval_secs: u64,
    /// Volatility used until the buffer has enough samples.
    pub fallback_vol: f64,
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
}

// -- Persisted state -----------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct AccountState {
    pub account_id: String,
    /// Optional: track which symbol we bootstrap-funded with, so a config
    /// change doesn't silently leave us under-funded.
    pub settlement_symbol: Option<String>,
}

fn load_account_state(p: &Path) -> Option<AccountState> {
    let bytes = std::fs::read(p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_account_state(p: &Path, state: &AccountState) -> Result<()> {
    let pretty = serde_json::to_vec_pretty(state)?;
    std::fs::write(p, pretty).context("writing account state sidecar")?;
    Ok(())
}

// -- Entry point ---------------------------------------------------------

/// Run the bot to completion (loop exits only on WS close). Identical
/// behaviour to invoking the binary; factored out so the chain test
/// harness can drive the real bot in-process.
pub async fn run(cli: Cli) -> Result<()> {
    let cfg = load_config(&cli.config)?;
    let secrets_loaded = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network.as_str())?;

    let underlying_spec = net.token_spec(&cfg.underlying_symbol).with_context(|| {
        format!(
            "underlying symbol {} not in deployments.token_info",
            cfg.underlying_symbol
        )
    })?;
    let settlement_spec = net.token_spec(&cfg.settlement_symbol).with_context(|| {
        format!(
            "settlement symbol {} not in deployments.token_info",
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

    let signer = load_quote_signer(&secrets_loaded, cfg.signing_scheme)?;
    let pubkey_bytes = signer.public_bytes();
    tracing::info!(scheme = ?signer.scheme(), pubkey_len = pubkey_bytes.len(), "quote signer ready");

    let account_id =
        resolve_account(&cli, &cfg, net, &secrets_loaded, &signer, &pubkey_bytes).await?;
    tracing::info!(account_id = %account_id, "mm account ready");

    let mut ws = ws_client::connect(&cfg.quoting_url).await?;
    let account_id_pt = pt_object_id_from_sui(account_id);
    ws_client::send_json(
        &mut ws,
        &MmToService::Hello {
            payload: MmHelloPayload {
                roles: cfg.roles.clone(),
                account_id: account_id_pt,
                signing_scheme: signer.scheme(),
                signing_pubkey: pubkey_bytes.clone(),
            },
        },
    )
    .await?;
    let challenge = expect_auth_challenge(&mut ws).await?;
    let sig = signer.sign(&challenge)?;
    ws_client::send_json(
        &mut ws,
        &MmToService::AuthResponse {
            payload: AuthResponsePayload { signature: sig },
        },
    )
    .await?;
    expect_auth_ack(&mut ws).await?;
    tracing::info!("authenticated with quoting service");

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building reqwest client")?;
    let price_cache = PriceCache::new();
    let stream_rx = pyth::spawn_subscriber(
        http_client.clone(),
        cfg.pyth.hermes_url.clone(),
        vec![underlying_feed, settlement_feed],
    );
    price_cache.spawn_updater(stream_rx);

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

    wait_for_first_prices(
        &price_cache,
        underlying_feed,
        settlement_feed,
        Duration::from_secs(30),
    )
    .await?;

    let token_recipient = resolve_token_recipient(&cfg, &secrets_loaded, cli.network)?;
    let protocol_id = net.protocol_id_bytes()?;
    let mut nonce_counter = now_ms();
    loop {
        let frame: ServiceToMm = match ws_client::next_json(&mut ws).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "ws closed");
                break;
            }
        };
        match frame {
            ServiceToMm::RFQBroadcast {
                request_id,
                payload,
            } => {
                tracing::debug!(
                    ?request_id,
                    strike = payload.strike,
                    expiry_ms = payload.expiry_ms,
                    write_amount = payload.write_amount,
                    "received rfq broadcast"
                );
                let now = now_ms();
                let ms_to_expiry = payload.expiry_ms.saturating_sub(now);
                let t_years = ms_to_expiry as f64 / 1000.0 / 86_400.0 / 365.0;

                let spot_scaled = match compute_spot(
                    &price_cache,
                    underlying_feed,
                    settlement_feed,
                    underlying_decimals,
                    settlement_decimals,
                    &cfg.pyth,
                ) {
                    Ok(s) => s,
                    Err(reason) => {
                        tracing::debug!(?request_id, %reason, "declining: stale market data");
                        ws_client::send_json(
                            &mut ws,
                            &MmToService::Decline {
                                request_id,
                                payload: shared::protocol_types::messages::DeclinePayload {
                                    reason: format!("stale market data: {reason}"),
                                },
                            },
                        )
                        .await?;
                        continue;
                    }
                };
                let sigma = vol_buf.read().current_annualized().unwrap_or(cfg.pyth.fallback_vol);

                let inputs = CallInputs {
                    spot: spot_scaled as f64,
                    strike: payload.strike as f64,
                    t_years,
                    r: cfg.rate,
                    sigma,
                };
                let per_unit = call_price_per_unit(inputs);
                let premium = premium_for_write(per_unit, payload.write_amount);

                tracing::debug!(
                    spot = spot_scaled,
                    sigma,
                    strike = payload.strike,
                    t_years,
                    per_unit,
                    write_amount = payload.write_amount,
                    premium,
                    "priced"
                );

                if premium == 0 {
                    tracing::debug!(?request_id, "priced to zero; declining");
                    ws_client::send_json(
                        &mut ws,
                        &MmToService::Decline {
                            request_id,
                            payload: shared::protocol_types::messages::DeclinePayload {
                                reason: "priced to zero".into(),
                            },
                        },
                    )
                    .await?;
                    continue;
                }

                let valid_until_ms = now + cfg.quote_ttl_ms;
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

                ws_client::send_json(
                    &mut ws,
                    &MmToService::Quote {
                        request_id,
                        payload: MmQuotePayload {
                            quote,
                            signature: sig,
                        },
                    },
                )
                .await?;
                tracing::info!(premium, nonce = nonce_counter, "quote sent");
            }
            ServiceToMm::Ping => {
                ws_client::send_json(&mut ws, &MmToService::Pong).await?;
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
    Ok(())
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
    secrets: &shared::Secrets,
    scheme: SigningScheme,
) -> Result<QuoteSigner> {
    QuoteSigner::from_secret_str(secrets.mm_quote_key()?, scheme)
}

async fn resolve_account(
    cli: &Cli,
    cfg: &BotConfig,
    net: &shared::deployments::NetworkDeployment,
    secrets: &shared::Secrets,
    signer: &QuoteSigner,
    pubkey_bytes: &[u8],
) -> Result<ObjectID> {
    if let Some(state) = load_account_state(&cli.account_state) {
        if let Some(prev) = &state.settlement_symbol {
            if prev != &cfg.settlement_symbol {
                tracing::warn!(
                    prev,
                    cfg.settlement_symbol,
                    "account was bootstrapped with a different settlement symbol — bot may quote without sufficient balance"
                );
            }
        }
        return ObjectID::from_hex_literal(&state.account_id).context("parsing account id");
    }

    tracing::info!("no account state — bootstrapping a fresh Account");
    let wrap = SuiClientWrapper::connect(secrets, cli.network).await?;
    let created = create_and_share_account(
        &wrap.client,
        &wrap.signer,
        net.package()?,
        signer.scheme(),
        pubkey_bytes,
        cli.gas_budget,
    )
    .await?;
    tracing::info!(digest = %created.digest, account_id = %created.account_id, "account created");

    let settlement = net.token(&cfg.settlement_symbol)?;
    let (tokens_pkg, settlement_module) = settlement.module_path()?;
    let fund_resp = mint_and_deposit_into_account(
        &wrap.client,
        &wrap.signer,
        tokens_pkg,
        &settlement_module,
        settlement.faucet()?,
        &settlement.coin_type,
        created.account_id,
        net.package()?,
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

    save_account_state(
        &cli.account_state,
        &AccountState {
            account_id: created.account_id.to_string(),
            settlement_symbol: Some(cfg.settlement_symbol.clone()),
        },
    )?;
    Ok(created.account_id)
}

async fn expect_auth_challenge(ws: &mut ws_client::WsStream) -> Result<Vec<u8>> {
    match ws_client::next_json::<ServiceToMm>(ws).await? {
        ServiceToMm::AuthChallenge { payload } => Ok(payload.challenge),
        other => Err(anyhow!("expected AuthChallenge, got {:?}", other)),
    }
}

async fn expect_auth_ack(ws: &mut ws_client::WsStream) -> Result<()> {
    match ws_client::next_json::<ServiceToMm>(ws).await? {
        ServiceToMm::AuthAck { .. } => Ok(()),
        ServiceToMm::Error { payload, .. } => Err(anyhow!(
            "auth rejected: {} — {}",
            payload.code,
            payload.message
        )),
        other => Err(anyhow!("expected AuthAck, got {:?}", other)),
    }
}

fn resolve_token_recipient(
    cfg: &BotConfig,
    secrets: &shared::Secrets,
    network: Network,
) -> Result<PtSuiAddress> {
    if let Some(s) = &cfg.token_recipient {
        tracing::debug!(recipient = %s, "using configured token recipient");
        return PtSuiAddress::from_hex(s).context("parsing token_recipient");
    }
    tracing::debug!("deriving token recipient from sui key");
    let raw = secrets.sui_private_key(network.as_str())?;
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

/// Live cross spot, scaled into the bucket's on-chain units:
/// settlement smallest-units per underlying smallest-unit.
fn compute_spot(
    cache: &PriceCache,
    underlying_feed: PriceFeedId,
    settlement_feed: PriceFeedId,
    underlying_decimals: u8,
    settlement_decimals: u8,
    cfg: &PythConfig,
) -> Result<u64, &'static str> {
    let local = Duration::from_millis(cfg.max_price_age_ms);
    let publish = Duration::from_millis(cfg.max_publish_lag_ms);
    let u = cache
        .get_fresh(underlying_feed, local, publish)
        .ok_or("underlying price stale or unseen")?;
    let s = cache
        .get_fresh(settlement_feed, local, publish)
        .ok_or("settlement price stale or unseen")?;
    if !(u.price.is_finite() && u.price > 0.0 && s.price.is_finite() && s.price > 0.0) {
        return Err("non-positive or non-finite price");
    }
    let cross = u.price / s.price;
    let scale = 10f64.powi(settlement_decimals as i32 - underlying_decimals as i32);
    let scaled = cross * scale;
    if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
        return Err("scaled spot out of range");
    }
    let spot = scaled.round() as u64;
    tracing::trace!(underlying_usd = u.price, settlement_usd = s.price, cross, spot, "computed spot");
    Ok(spot)
}

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

fn spawn_vol_task(
    client: reqwest::Client,
    cfg: PythConfig,
    underlying_feed: PriceFeedId,
    cache: PriceCache,
    buf: Arc<RwLock<RollingVolBuffer>>,
) {
    tokio::spawn(async move {
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

        let mut ticker = tokio::time::interval(Duration::from_millis(cfg.vol_sample_interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let Some(cp) = cache.peek(underlying_feed) else {
                continue;
            };
            if cp.observed_at.elapsed() > Duration::from_millis(cfg.max_price_age_ms) {
                continue;
            }
            buf.write().push(now_ms(), cp.price);
            if let Some(sigma) = buf.read().current_annualized() {
                tracing::debug!(sigma, samples = buf.read().len(), "vol updated");
            }
        }
    });
}
