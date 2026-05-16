//! Basic market-maker bot.
//!
//! Phase 1: bootstrap. On first run, reads its TOML config and the
//! `MM_QUOTE_KEY` env var (32-byte Ed25519 secret as hex, with optional
//! `0x` prefix), then calls `account::create_and_share_account(pubkey)`
//! and persists the resulting Account id to `mm-bot.account.json` next to
//! the config so the next run skips this step.
//!
//! Phase 2: serve. Opens a WS to the quoting service, authenticates with
//! the Ed25519 challenge-response (§5.4.1), then loops:
//!
//!   - `RFQBroadcast` arrives.
//!   - Compute `t_years = max(0, expiry - now) / 365.0` from the bucket's
//!     expiry (looked up over WS via `SubscribeBuckets` cached state, or
//!     skipped on the first request — see TODO).
//!   - Black-Scholes call price → `premium = floor(per_unit * write_amount)`.
//!   - Build `Quote`, BCS-encode, Ed25519-sign, send back.
//!
//! For the MVP the bot serves as a **Trader MM** (buys writer-side RFQs).
//! Adding writer-side serving is purely a connection-time `roles` flag
//! change; we keep both supported.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ed25519_dalek::{Signer as EdSigner, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;

use protocol_types::ids::{ObjectId as PtObjectId, SuiAddress as PtSuiAddress};
use protocol_types::messages::{
    AuthResponsePayload, MmHelloPayload, MmQuotePayload, MmToService, ServiceToMm,
};
use protocol_types::quote::Quote;
use protocol_types::sides::MmRole;

use clients::deployments::Deployments;
use clients::pricing::{call_price_per_unit, premium_for_write, CallInputs};
use clients::sui_client::{Network, SuiClientWrapper};
use clients::tx::account::create_and_share_account;
use clients::ws_client;

// -- Config --------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct BotConfig {
    /// Quoting-service WS URL.
    quoting_url: String,
    /// Per-bucket inputs (we keep it simple: one config covers any bucket
    /// the bot quotes for). The user fills these in based on what their
    /// model thinks for the asset pair under test.
    spot_price: u64,
    /// Annualized vol (e.g. `0.6` for 60%).
    vol: f64,
    /// Annualized risk-free rate.
    #[serde(default)]
    rate: f64,
    /// Assumed time-to-expiry in days. Used until we wire bucket reads.
    days_to_expiry: f64,
    /// Quote validity window from sign time, in milliseconds.
    #[serde(default = "default_quote_ttl_ms")]
    quote_ttl_ms: u64,
    /// Roles to advertise to the quoting service.
    roles: Vec<MmRole>,
    /// Where to receive minted call tokens (writer flow) or position NFTs
    /// (trader flow). Defaults to our Sui address.
    #[serde(default)]
    token_recipient: Option<String>,
}

fn default_quote_ttl_ms() -> u64 {
    30_000
}

#[derive(Parser)]
#[command(name = "mm-bot", about = "Test market-maker bot for the options protocol")]
struct Cli {
    /// Bot TOML config.
    #[arg(short, long, default_value = "config/mm-bot.toml")]
    config: PathBuf,

    /// Sidecar file persisting the auto-bootstrapped Account id.
    #[arg(long, default_value = "mm-bot.account.json")]
    account_state: PathBuf,

    /// Path to deployments.json.
    #[arg(short, long, default_value = "deployments.json")]
    deployments: PathBuf,

    /// Target network.
    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    /// Gas budget for the bootstrap Account-creation tx.
    #[arg(long, default_value_t = 100_000_000)]
    gas_budget: u64,
}

// -- Persisted state -----------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct AccountState {
    account_id: String,
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

    // Ed25519 signing key for Quotes (separate semantics from the Sui wallet
    // key, though for Ed25519 Sui keypairs they can be the same value).
    let signing_key = load_quote_signing_key()?;
    let verifying_key: VerifyingKey = signing_key.verifying_key();
    let pubkey_bytes = verifying_key.to_bytes().to_vec();

    // Bootstrap (or load) the on-chain Account.
    let account_id = resolve_account(&cli, &cfg, &pubkey_bytes).await?;
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
                signing_pubkey: pubkey_bytes.clone(),
            },
        },
    )
    .await?;

    // AuthChallenge → AuthResponse → AuthAck.
    let challenge = expect_auth_challenge(&mut ws).await?;
    let sig = signing_key.sign(&challenge).to_bytes().to_vec();
    ws_client::send_json(
        &mut ws,
        &MmToService::AuthResponse {
            payload: AuthResponsePayload { signature: sig },
        },
    )
    .await?;
    expect_auth_ack(&mut ws).await?;
    tracing::info!("authenticated with quoting service");

    // Quote-signing loop.
    let token_recipient = resolve_token_recipient(&cfg)?;
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
                let inputs = CallInputs {
                    spot: cfg.spot_price as f64,
                    // Strike is on the bucket — we'd need to read the bucket
                    // to know it. As a placeholder use spot (ATM); in
                    // production wire a SubscribeBuckets / strike cache.
                    strike: cfg.spot_price as f64,
                    t_years: cfg.days_to_expiry / 365.0,
                    r: cfg.rate,
                    sigma: cfg.vol,
                };
                let per_unit = call_price_per_unit(inputs);
                let premium = premium_for_write(per_unit, payload.write_amount);

                if premium == 0 {
                    tracing::debug!(?request_id, "priced to zero; declining");
                    ws_client::send_json(
                        &mut ws,
                        &MmToService::Decline {
                            request_id,
                            payload: protocol_types::messages::DeclinePayload {
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
                    protocol_id: load_protocol_id(&cli)?,
                    signer_account_id: account_id_pt,
                    signer_token_recipient: token_recipient,
                    bucket_id: payload.bucket_id,
                    write_amount: payload.write_amount,
                    premium,
                    valid_until_ms,
                    nonce: nonce_counter,
                };
                let bytes = quote.to_bcs_bytes()?;
                let sig = signing_key.sign(&bytes).to_bytes().to_vec();

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
                // Observed only — a real bot would update its inventory model.
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
    settings
        .try_deserialize::<BotConfig>()
        .with_context(|| format!("parsing {}", path.display()))
}

fn load_quote_signing_key() -> Result<SigningKey> {
    let raw =
        std::env::var("MM_QUOTE_KEY").context("MM_QUOTE_KEY env var not set")?;
    let stripped = raw.trim().strip_prefix("0x").unwrap_or(raw.trim());
    let bytes =
        hex::decode(stripped).context("MM_QUOTE_KEY is not hex")?;
    if bytes.len() != 32 {
        return Err(anyhow!(
            "MM_QUOTE_KEY must be 32 bytes of Ed25519 secret; got {}",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

async fn resolve_account(
    cli: &Cli,
    _cfg: &BotConfig,
    pubkey_bytes: &[u8],
) -> Result<ObjectID> {
    if let Some(state) = load_account_state(&cli.account_state) {
        return ObjectID::from_hex_literal(&state.account_id).context("parsing account id");
    }
    // Bootstrap.
    tracing::info!("no account state — bootstrapping a fresh Account");
    let dep = Deployments::load(&cli.deployments)?;
    let net = dep.for_network(cli.network)?;
    let wrap = SuiClientWrapper::connect(cli.network).await?;
    let created = create_and_share_account(
        &wrap.client,
        &wrap.signer,
        net.package()?,
        pubkey_bytes,
        cli.gas_budget,
    )
    .await?;
    save_account_state(
        &cli.account_state,
        &AccountState {
            account_id: created.account_id.to_string(),
        },
    )?;
    tracing::info!(digest = %created.digest, account_id = %created.account_id, "account created");
    Ok(created.account_id)
}

async fn expect_auth_challenge(
    ws: &mut ws_client::WsStream,
) -> Result<Vec<u8>> {
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

fn resolve_token_recipient(cfg: &BotConfig) -> Result<PtSuiAddress> {
    if let Some(s) = &cfg.token_recipient {
        return PtSuiAddress::from_hex(s).context("parsing token_recipient");
    }
    // Default: our Sui address. Pull from env so we don't need a Sui RPC
    // round-trip for this string.
    let net = std::env::var("SUI_PRIVATE_KEY_TESTNET")
        .or_else(|_| std::env::var("SUI_PRIVATE_KEY"))
        .map_err(|_| anyhow!("token_recipient unset and no SUI_PRIVATE_KEY found to derive one"))?;
    let kp = sui_types::crypto::SuiKeyPair::decode(net.trim())
        .map_err(|e| anyhow!("decoding sui key: {e}"))?;
    let addr = sui_types::base_types::SuiAddress::from(&kp.public());
    PtSuiAddress::from_hex(&addr.to_string()).context("converting sui address")
}

fn load_protocol_id(cli: &Cli) -> Result<Vec<u8>> {
    let dep = Deployments::load(&cli.deployments)?;
    let net = dep.for_network(cli.network)?;
    net.protocol_id_bytes()
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
