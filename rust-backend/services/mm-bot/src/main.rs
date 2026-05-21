//! Market-maker bot.
//!
//! Phase 1: bootstrap.
//!   - Reads its TOML config (incl. `signing_scheme`) + `MM_QUOTE_KEY`
//!     (32-byte hex secret — interpretation depends on the scheme).
//!   - Resolves the test-token pair (`underlying_symbol` / `settlement_symbol`)
//!     against `deployments.json`.
//!   - If no `mm-bot.account.json` exists, calls
//!     `account::create_and_share_account(scheme, pubkey)` and persists the
//!     new Account id, then funds it with `bootstrap_settlement_amount` of
//!     the settlement asset via `test_tokens::<sym>::mint` +
//!     `account::deposit` in a single PTB.
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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
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
use shared::quote_signer::QuoteSigner;
use shared::sui_client::{Network, SuiClientWrapper};
use shared::tx::account::create_and_share_account;
use shared::tx::test_tokens::mint_and_deposit_into_account;
use shared::ws_client;

use mm_bot::Cli;

// -- Config --------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct BotConfig {
    quoting_url: String,

    /// Quote-signing scheme. Stored on chain alongside the pubkey; the
    /// `MM_QUOTE_KEY` env var holds the 32-byte secret in this scheme.
    /// One of `ed25519` / `secp256k1` / `secp256r1`.
    #[serde(default = "default_scheme")]
    signing_scheme: SigningScheme,

    /// Asset pair to quote on. Symbols are looked up in
    /// `deployments.json::testTokens.tokens`.
    #[serde(default = "default_underlying")]
    underlying_symbol: String,
    #[serde(default = "default_settlement")]
    settlement_symbol: String,

    /// Pricing inputs. `spot_price` is in the **same units as the
    /// bucket's `strike` on chain**: settlement smallest-units per
    /// underlying smallest-unit. For BTC at $50k with TUSDC (6 dec) and
    /// TBTC (8 dec), that's `50_000 * 10^6 / 10^8 = 500`.
    spot_price: u64,
    vol: f64,
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

// -- Persisted state -----------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct AccountState {
    account_id: String,
    /// Optional: track which symbol we bootstrap-funded with, so a config
    /// change doesn't silently leave us under-funded.
    settlement_symbol: Option<String>,
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

// -- Main loop -----------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = load_config(&cli.config)?;
    let secrets_loaded = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network.as_str())?;

    // Token lookup so we fail fast on a typoed symbol.
    let settlement = net.token(&cfg.settlement_symbol).with_context(|| {
        format!(
            "settlement symbol {} not in deployments.testTokens",
            cfg.settlement_symbol
        )
    })?;
    let _underlying = net.token(&cfg.underlying_symbol).with_context(|| {
        format!(
            "underlying symbol {} not in deployments.testTokens",
            cfg.underlying_symbol
        )
    })?;

    // Quote-signing key (scheme-aware) from the secrets TOML.
    let signer = load_quote_signer(&secrets_loaded, cfg.signing_scheme)?;
    let pubkey_bytes = signer.public_bytes();
    tracing::info!(scheme = ?signer.scheme(), pubkey_len = pubkey_bytes.len(), "quote signer ready");

    // Account: bootstrap if missing, then fund with settlement.
    let account_id =
        resolve_account(&cli, &cfg, net, &secrets_loaded, &signer, &pubkey_bytes).await?;
    tracing::info!(account_id = %account_id, "mm account ready");

    // Connect + auth to the quoting service.
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

    // Quote loop.
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
                let now = now_ms();
                // Time to expiry from the bucket's expiry_ms — saturates
                // at zero so an already-expired bucket prices to intrinsic.
                let ms_to_expiry = payload.expiry_ms.saturating_sub(now);
                let t_years = ms_to_expiry as f64 / 1000.0 / 86_400.0 / 365.0;
                let inputs = CallInputs {
                    spot: cfg.spot_price as f64,
                    strike: payload.strike as f64,
                    t_years,
                    r: cfg.rate,
                    sigma: cfg.vol,
                };
                let per_unit = call_price_per_unit(inputs);
                let premium = premium_for_write(per_unit, payload.write_amount);

                tracing::debug!(
                    spot = cfg.spot_price,
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
            ServiceToMm::AccountStateUpdate { .. }
            | ServiceToMm::ReservationConfirmed { .. }
            | ServiceToMm::ReservationReleased { .. } => {
                // Observed-only for now.
            }
            other => {
                tracing::debug!(?other, "ignored frame");
            }
        }
    }
    // Settlement is referenced through the bootstrap path; suppress
    // unused-binding warning when bootstrap path didn't run.
    let _ = settlement;
    Ok(())
}

// -- helpers -------------------------------------------------------------

fn load_config(path: &Path) -> Result<BotConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::from(path).required(true))
        .build()
        .with_context(|| format!("loading {}", path.display()))?;
    settings
        .try_deserialize::<BotConfig>()
        .with_context(|| format!("parsing {}", path.display()))
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

    // Fund it with settlement so it can pay premiums on day one.
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
        return PtSuiAddress::from_hex(s).context("parsing token_recipient");
    }
    // Derive the address from the same Sui key the bot signs gas with.
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
