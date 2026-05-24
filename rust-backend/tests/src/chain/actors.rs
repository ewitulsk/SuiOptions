//! Test actors that drive a [`super::env::TestEnv`].
//!
//! - [`MockMm`] — a precise WS-level mock the test can fully control.
//!   Creates a real on-chain Account (so the quoting service's auth check
//!   passes against the actual indexer-observed pubkey) and lets the test
//!   sign whatever quote the scenario calls for.
//! - [`Writer`] — drives `shared::tx::execute_write::execute_writer_flow`
//!   end-to-end. Lighter than reusing the `writer` binary's `run` because
//!   the binary's CLI hardcodes the public-network RPC URLs.
//! - [`RealMmBot`] is intentionally absent: the real `mm-bot::run` needs
//!   live Pyth feeds which the harness doesn't (yet) mock. The Phase 3
//!   happy-path scenario uses [`MockMm`] with a hard-coded premium —
//!   sufficient to verify every other layer (PTB submission, indexer
//!   ingestion, balance mirroring) end-to-end.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer as EdSigner, SigningKey};
use futures_util::{SinkExt, StreamExt};
use sui_json_rpc_types::{ObjectChange, SuiTransactionBlockEffectsAPI};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::crypto::{get_account_key_pair, SuiKeyPair};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info};

use shared::protocol_types::ids::{ObjectId as PtObjectId, SuiAddress as PtSuiAddress};
use shared::protocol_types::messages::{
    AuthResponsePayload, DeclinePayload, MmHelloPayload, MmQuotePayload, MmToService,
    RfqBroadcastPayload, ServiceToMm,
};
use shared::protocol_types::quote::Quote;
use shared::protocol_types::sides::MmRole;
use shared::protocol_types::SigningScheme;
use shared::sui_client::Signer;
use shared::tx::account::create_and_share_account;
use shared::tx::execute_write::{execute_writer_flow, ExecuteWriteParams};
use shared::tx::test_tokens::mint_and_deposit_into_account;

use super::env::TestEnv;

const ACTOR_GAS_BUDGET: u64 = 200_000_000;

pub type WsStream = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

/// Mock MM with full control over what it signs. Holds a Sui keypair
/// (gas + Account owner) and an ed25519 quote-signing key (registered on
/// chain at `create_and_share_account` time).
pub struct MockMm {
    pub sui_keypair: SuiKeyPair,
    pub sui_address: SuiAddress,
    pub quote_sk: SigningKey,
    pub account_id: ObjectID,
    pub ws: WsStream,
}

impl MockMm {
    /// Bootstrap a fresh MM:
    ///
    /// 1. Generate a Sui keypair, fund it via the localnet faucet.
    /// 2. Generate an Ed25519 quote-signing key.
    /// 3. Call `account::create_and_share_account(scheme, pubkey)` to
    ///    create the on-chain Account (registers the pubkey).
    /// 4. Mint+deposit `usdc_balance` of TUSDC into the Account.
    /// 5. Wait for the indexer + quoting service to mirror the Account.
    /// 6. Connect+auth to the quoting service WS as `roles`.
    pub async fn bootstrap(
        env: &TestEnv,
        roles: Vec<MmRole>,
        usdc_balance: u64,
    ) -> Result<Self> {
        let (sui_address, ed) = get_account_key_pair();
        let sui_keypair = SuiKeyPair::Ed25519(ed);
        fund_via_faucet(&env.localnet().faucet_url, sui_address).await?;
        wait_for_funds(env.client(), sui_address, Duration::from_secs(15)).await?;

        let quote_sk = SigningKey::generate(&mut rand::rngs::OsRng);
        let pubkey_bytes = quote_sk.verifying_key().to_bytes().to_vec();

        let mm_signer = Signer {
            keypair: clone_keypair(&sui_keypair)?,
            address: sui_address,
        };
        let dep = env.deployments();
        let net = dep.for_network("devnet")?;
        let package = net.package()?;

        let created = create_and_share_account(
            env.client(),
            &mm_signer,
            package,
            SigningScheme::Ed25519,
            &pubkey_bytes,
            ACTOR_GAS_BUDGET,
        )
        .await
        .context("creating on-chain MM account")?;
        info!(account_id = %created.account_id, "mock mm account created");

        if usdc_balance > 0 {
            let tusdc = net.token("TUSDC")?;
            let (tokens_pkg, module) = tusdc.module_path()?;
            mint_and_deposit_into_account(
                env.client(),
                &mm_signer,
                tokens_pkg,
                &module,
                tusdc.faucet()?,
                &tusdc.coin_type,
                created.account_id,
                package,
                usdc_balance,
                ACTOR_GAS_BUDGET,
            )
            .await
            .context("funding MM account with TUSDC")?;
        }

        // Wait for the indexer + quoting service to observe the new
        // Account + its USDC balance before authenticating.
        let mut acct_bytes = [0u8; 32];
        acct_bytes.copy_from_slice(created.account_id.into_bytes().as_ref());
        super::services::wait_for_account_propagation(
            env.indexer(),
            env.quoting(),
            acct_bytes,
            Duration::from_secs(15),
        )
        .await?;

        let ws = connect_and_auth(env, created.account_id, &quote_sk, roles).await?;

        Ok(Self {
            sui_keypair,
            sui_address,
            quote_sk,
            account_id: created.account_id,
            ws,
        })
    }

    /// Block on the next non-Ping frame from the quoting service.
    pub async fn next_frame(&mut self) -> Result<ServiceToMm> {
        loop {
            let msg = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow!("mm ws closed"))??;
            let text = match msg {
                Message::Text(t) => t,
                Message::Binary(b) => String::from_utf8(b)?,
                Message::Close(_) => return Err(anyhow!("mm ws closed")),
                _ => continue,
            };
            let frame: ServiceToMm = serde_json::from_str(&text)?;
            if matches!(frame, ServiceToMm::Ping) {
                // Honor the heartbeat so the service doesn't disconnect us.
                self.send(&MmToService::Pong).await?;
                continue;
            }
            return Ok(frame);
        }
    }

    /// Wait for an `RFQBroadcast` specifically.
    pub async fn await_rfq(&mut self) -> Result<(String, RfqBroadcastPayload)> {
        match self.next_frame().await? {
            ServiceToMm::RFQBroadcast { request_id, payload } => Ok((request_id, payload)),
            other => Err(anyhow!("expected RFQBroadcast, got {:?}", other)),
        }
    }

    /// Sign a `Quote` over BCS bytes using the registered Ed25519 key and
    /// send it to the service.
    pub async fn send_signed_quote(
        &mut self,
        request_id: &str,
        bucket: ObjectID,
        write_amount: u64,
        premium: u64,
        nonce: u64,
        valid_until_ms: u64,
        protocol_id: Vec<u8>,
    ) -> Result<()> {
        let bucket_pt = pt_id(bucket);
        let account_pt = pt_id(self.account_id);
        // Receiver of the call token: the MM's own Sui address.
        let token_recipient = pt_addr(self.sui_address)?;
        let quote = Quote {
            protocol_id,
            signer_account_id: account_pt,
            signer_token_recipient: token_recipient,
            bucket_id: bucket_pt,
            write_amount,
            premium,
            valid_until_ms,
            nonce,
        };
        let sig = self.quote_sk.sign(&quote.to_bcs_bytes()?).to_bytes().to_vec();
        self.send(&MmToService::Quote {
            request_id: request_id.to_string(),
            payload: MmQuotePayload { quote, signature: sig },
        })
        .await
    }

    pub async fn send_decline(&mut self, request_id: &str, reason: &str) -> Result<()> {
        self.send(&MmToService::Decline {
            request_id: request_id.to_string(),
            payload: DeclinePayload {
                reason: reason.to_string(),
            },
        })
        .await
    }

    async fn send(&mut self, msg: &MmToService) -> Result<()> {
        self.ws
            .send(Message::Text(serde_json::to_string(msg)?))
            .await?;
        Ok(())
    }
}

/// Retail writer actor. Owns a Sui keypair (funded via the faucet) used
/// to pay gas and receive the PositionNFT. Drives one §3.3.4 writer-flow
/// PTB per `execute(...)` call.
pub struct Writer {
    pub sui_keypair: SuiKeyPair,
    pub sui_address: SuiAddress,
}

impl Writer {
    pub async fn bootstrap(env: &TestEnv) -> Result<Self> {
        let (sui_address, ed) = get_account_key_pair();
        let sui_keypair = SuiKeyPair::Ed25519(ed);
        fund_via_faucet(&env.localnet().faucet_url, sui_address).await?;
        wait_for_funds(env.client(), sui_address, Duration::from_secs(15)).await?;
        Ok(Self {
            sui_keypair,
            sui_address,
        })
    }

    /// Execute one §3.3.4 writer-flow PTB. `mm_account_id` is the
    /// signer-side Account; the rest comes from the chosen quote.
    pub async fn execute(
        &self,
        env: &TestEnv,
        bucket: ObjectID,
        underlying_symbol: &str,
        settlement_symbol: &str,
        mm_account_id: ObjectID,
        signed_quote: &shared::protocol_types::messages::RfqQuoteEntry,
    ) -> Result<String> {
        let dep = env.deployments();
        let net = dep.for_network("devnet")?;
        let package = net.package()?;
        let protocol_config = net.protocol_config()?;
        let treasury = net.treasury()?;
        let underlying = net.token(underlying_symbol)?;
        let settlement = net.token(settlement_symbol)?;
        let (tokens_pkg, underlying_module) = underlying.module_path()?;

        let signer = Signer {
            keypair: clone_keypair(&self.sui_keypair)?,
            address: self.sui_address,
        };

        let signer_token_recipient = sui_addr_from_pt(signed_quote.quote.signer_token_recipient)?;
        let bucket_id_bytes: [u8; 32] = *signed_quote.quote.bucket_id.as_bytes();
        let signer_account_id_bytes: [u8; 32] = *signed_quote.quote.signer_account_id.as_bytes();

        let params = ExecuteWriteParams {
            package,
            underlying_type: &underlying.coin_type,
            settlement_type: &settlement.coin_type,
            tokens_package: tokens_pkg,
            underlying_module: &underlying_module,
            underlying_faucet_id: underlying.faucet()?,
            bucket_id: bucket,
            protocol_config_id: protocol_config,
            treasury_id: treasury,
            mm_account_id,
            protocol_id: signed_quote.quote.protocol_id.clone(),
            signer_account_id_bytes,
            signer_token_recipient,
            bucket_id_bytes,
            write_amount: signed_quote.quote.write_amount,
            premium: signed_quote.quote.premium,
            valid_until_ms: signed_quote.quote.valid_until_ms,
            nonce: signed_quote.quote.nonce,
            signature: signed_quote.signature.clone(),
            position_nft_recipient: self.sui_address,
            call_token_recipient: signer_token_recipient,
            gas_budget: ACTOR_GAS_BUDGET,
        };
        let resp = execute_writer_flow(env.client(), &signer, &params).await?;
        let digest = resp.digest.to_string();
        debug!(%digest, "writer-flow PTB landed");
        Ok(digest)
    }
}

// -- helpers --------------------------------------------------------------

async fn connect_and_auth(
    env: &TestEnv,
    account_id: ObjectID,
    quote_sk: &SigningKey,
    roles: Vec<MmRole>,
) -> Result<WsStream> {
    let addr = env.quoting().bind_addr;
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://{}/", addr)).await?;

    let hello = MmToService::Hello {
        payload: MmHelloPayload {
            roles,
            account_id: pt_id(account_id),
            signing_scheme: SigningScheme::Ed25519,
            signing_pubkey: quote_sk.verifying_key().to_bytes().to_vec(),
        },
    };
    ws.send(Message::Text(serde_json::to_string(&hello)?)).await?;

    // Challenge.
    let frame = next_text(&mut ws).await?;
    let parsed: ServiceToMm = serde_json::from_str(&frame)?;
    let challenge = match parsed {
        ServiceToMm::AuthChallenge { payload } => payload.challenge,
        other => return Err(anyhow!("expected AuthChallenge, got {:?}", other)),
    };
    let sig = quote_sk.sign(&challenge).to_bytes().to_vec();
    let resp = MmToService::AuthResponse {
        payload: AuthResponsePayload { signature: sig },
    };
    ws.send(Message::Text(serde_json::to_string(&resp)?)).await?;
    // Ack.
    let frame = next_text(&mut ws).await?;
    let parsed: ServiceToMm = serde_json::from_str(&frame)?;
    match parsed {
        ServiceToMm::AuthAck { .. } => Ok(ws),
        other => Err(anyhow!("expected AuthAck, got {:?}", other)),
    }
}

async fn next_text(ws: &mut WsStream) -> Result<String> {
    loop {
        let frame = ws.next().await.ok_or_else(|| anyhow!("ws closed"))??;
        match frame {
            Message::Text(t) => return Ok(t),
            Message::Binary(b) => return Ok(String::from_utf8(b)?),
            Message::Close(_) => return Err(anyhow!("ws closed")),
            _ => continue,
        }
    }
}

async fn fund_via_faucet(faucet_url: &str, recipient: SuiAddress) -> Result<()> {
    let url = format!("{faucet_url}/v2/gas");
    let body = serde_json::json!({
        "FixedAmountRequest": { "recipient": recipient.to_string() }
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client.post(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "faucet refused: status {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    Ok(())
}

async fn wait_for_funds(
    client: &SuiClient,
    address: SuiAddress,
    timeout: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(p) = client
            .coin_read_api()
            .get_coins(address, None, None, Some(1))
            .await
        {
            if !p.data.is_empty() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(anyhow!("gas never landed for {address}"))
}

/// `SuiKeyPair` isn't `Clone`. Roundtrip via bech32 to produce an
/// independent copy when we need one (gas-paying Signer + on-chain
/// owner share the same key in tests).
fn clone_keypair(kp: &SuiKeyPair) -> Result<SuiKeyPair> {
    let s = kp
        .encode()
        .map_err(|e| anyhow!("encoding keypair: {e}"))?;
    SuiKeyPair::decode(&s).map_err(|e| anyhow!("re-decoding keypair: {e}"))
}

fn pt_id(id: ObjectID) -> PtObjectId {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(id.into_bytes().as_ref());
    PtObjectId::new(bytes)
}

fn pt_addr(a: SuiAddress) -> Result<PtSuiAddress> {
    PtSuiAddress::from_hex(&a.to_string()).context("converting SuiAddress")
}

fn sui_addr_from_pt(a: PtSuiAddress) -> Result<SuiAddress> {
    SuiAddress::from_str(&a.to_hex()).context("converting SuiAddress")
}

/// Find a `Created` object change of the given module+name in a tx
/// response. Used to extract the freshly-created `Bucket` id after an
/// admin `new_call_option` call.
pub fn extract_created_object(
    changes: &[ObjectChange],
    module: &str,
    name: &str,
) -> Option<ObjectID> {
    changes.iter().find_map(|c| match c {
        ObjectChange::Created {
            object_id,
            object_type,
            ..
        } if object_type.module.as_str() == module && object_type.name.as_str() == name => {
            Some(*object_id)
        }
        _ => None,
    })
}

/// Block until `resp` shows on-chain success (i.e. has a successful
/// effects status). Surfaces a clear error if it didn't.
pub fn assert_tx_success(
    resp: &sui_json_rpc_types::SuiTransactionBlockResponse,
) -> Result<()> {
    let effects = resp.effects.as_ref().ok_or_else(|| anyhow!("no effects"))?;
    if effects.status().is_err() {
        return Err(anyhow!("tx reverted: {:?}", effects.status()));
    }
    Ok(())
}
