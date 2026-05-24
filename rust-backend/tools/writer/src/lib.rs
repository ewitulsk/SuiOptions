//! Library surface for the `writer` binary.
//!
//! The §8.1 writer flow lives in [`run`] so the chain test harness can
//! drive it in-process. `main.rs` is a thin clap shim.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use sui_types::base_types::ObjectID;

use shared::deployments::Deployments;
use shared::protocol_types::ids::ObjectId as PtObjectId;
use shared::protocol_types::messages::{
    RetailHelloPayload, RetailToService, RfqRequestPayload, ServiceToRetail,
};
use shared::protocol_types::sides::{RetailRole, Side};
use shared::sui_client::{Network, SuiClientWrapper};
use shared::tx::execute_write::{execute_writer_flow, ExecuteWriteParams};
use shared::ws_client;

#[derive(Parser, Debug)]
#[command(name = "writer", about = "Retail-writer test client for the options protocol")]
pub struct Cli {
    #[arg(short, long, default_value = "deployments.json")]
    pub deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/writer/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(short = 'q', long, default_value = "ws://127.0.0.1:9002/")]
    pub quoting_url: String,

    /// Bucket id we're writing into.
    #[arg(short, long)]
    pub bucket: ObjectID,

    /// Underlying amount we're writing, in raw smallest-units (see token
    /// decimals in `deployments.json::testTokens`).
    #[arg(short = 'w', long)]
    pub write_amount: u64,

    /// Symbol for the underlying token (TBTC, TDEEP, TUSDC, TWAL).
    #[arg(long, default_value = "TBTC")]
    pub underlying: String,

    /// Symbol for the settlement token.
    #[arg(long, default_value = "TUSDC")]
    pub settlement: String,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    #[arg(long, default_value_t = 5)]
    pub rfq_timeout_secs: u64,
}

shared::define_program! {
    id          = "writer",
    cargo_pkg   = "writer",
    working_dir = ".",
    description = "Retail-writer test client. Walks the §8.1 writer flow end to end: RFQ \
                   over WS, pick best quote, submit one PTB that mints underlying via the \
                   faucet and calls bucket::execute_write.",
    cli         = crate::Cli,
}

/// Outcome of one writer-flow run: the digest of the landed
/// `execute_write` PTB. Returned so tests can correlate against indexer
/// events without re-parsing stdout.
pub struct WriteOutcome {
    pub digest: String,
}

/// Drive the §8.1 writer flow end-to-end. Connects to the quoting
/// service, fires one RFQ, picks the best quote, and submits the
/// corresponding `execute_write` PTB on chain.
pub async fn run(cli: Cli) -> Result<WriteOutcome> {
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network.as_str())?;

    let package = net.package()?;
    let protocol_config = net.protocol_config()?;
    let treasury = net.treasury().context("treasury_id missing from deployments")?;

    let underlying = net.token(&cli.underlying)?;
    let settlement = net.token(&cli.settlement)?;
    let (tokens_pkg, underlying_module) = underlying.module_path()?;

    let secrets = shared::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    let writer_addr = wrap.signer.address;
    tracing::info!(%writer_addr, underlying = %cli.underlying, settlement = %cli.settlement, "writer ready");

    // -- WS handshake -----------------------------------------------------
    let mut ws = ws_client::connect(&cli.quoting_url).await?;
    ws_client::send_json(
        &mut ws,
        &RetailToService::Hello {
            payload: RetailHelloPayload {
                role: RetailRole::Writer,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        },
    )
    .await?;
    let ack: ServiceToRetail = ws_client::next_json(&mut ws).await?;
    match ack {
        ServiceToRetail::HelloAck { .. } => {}
        other => return Err(anyhow!("expected HelloAck, got {:?}", other)),
    }

    // -- RFQ --------------------------------------------------------------
    let request_id = uuid::Uuid::new_v4().to_string();
    let bucket_pt = pt_object_id_from_sui(cli.bucket);
    ws_client::send_json(
        &mut ws,
        &RetailToService::RFQRequest {
            request_id: request_id.clone(),
            payload: RfqRequestPayload {
                bucket_id: bucket_pt,
                write_amount: cli.write_amount,
                side: Side::Writer,
            },
        },
    )
    .await?;

    let resp: ServiceToRetail =
        ws_client::next_json_timeout(&mut ws, Duration::from_secs(cli.rfq_timeout_secs)).await?;
    let payload = match resp {
        ServiceToRetail::RFQResponse {
            request_id: rid,
            payload,
        } => {
            if rid != request_id {
                return Err(anyhow!("response request_id {rid} ≠ {request_id}"));
            }
            payload
        }
        ServiceToRetail::Error { payload, .. } => {
            return Err(anyhow!("rfq error: {} — {}", payload.code, payload.message));
        }
        other => return Err(anyhow!("unexpected frame: {:?}", other)),
    };
    let best = payload
        .quotes
        .first()
        .ok_or_else(|| anyhow!("service returned no quotes"))?;
    tracing::info!(
        premium = best.quote.premium,
        mm_id = %best.mm_id,
        mm_rep = best.mm_reputation,
        "selected best quote"
    );

    // -- Execute write ----------------------------------------------------
    let mm_account_id = sui_object_id_from_pt(best.mm_id)?;
    let bucket_id_bytes: [u8; 32] = *best.quote.bucket_id.as_bytes();
    let signer_account_id_bytes: [u8; 32] = *best.quote.signer_account_id.as_bytes();
    let signer_token_recipient = sui_address_from_pt(best.quote.signer_token_recipient)?;

    let params = ExecuteWriteParams {
        package,
        underlying_type: &underlying.coin_type,
        settlement_type: &settlement.coin_type,
        tokens_package: tokens_pkg,
        underlying_module: &underlying_module,
        underlying_faucet_id: underlying.faucet()?,
        bucket_id: cli.bucket,
        protocol_config_id: protocol_config,
        treasury_id: treasury,
        mm_account_id,
        protocol_id: best.quote.protocol_id.clone(),
        signer_account_id_bytes,
        signer_token_recipient,
        bucket_id_bytes,
        write_amount: best.quote.write_amount,
        premium: best.quote.premium,
        valid_until_ms: best.quote.valid_until_ms,
        nonce: best.quote.nonce,
        signature: best.signature.clone(),
        position_nft_recipient: writer_addr,
        // Writer flow requires signer_token_recipient == call_token_recipient.
        call_token_recipient: signer_token_recipient,
        gas_budget: cli.gas_budget,
    };

    let resp = execute_writer_flow(&wrap.client, &wrap.signer, &params).await?;
    let digest = resp.digest.to_string();
    tracing::info!(%digest, "execute_write succeeded");
    Ok(WriteOutcome { digest })
}

// -- protocol-types ↔ sui-types id bridges -------------------------------

fn pt_object_id_from_sui(id: ObjectID) -> PtObjectId {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(id.into_bytes().as_ref());
    PtObjectId::new(bytes)
}

fn sui_object_id_from_pt(id: PtObjectId) -> Result<ObjectID> {
    ObjectID::from_str(&id.to_hex()).context("converting ObjectId to sui ObjectID")
}

fn sui_address_from_pt(
    addr: shared::protocol_types::ids::SuiAddress,
) -> Result<sui_types::base_types::SuiAddress> {
    sui_types::base_types::SuiAddress::from_str(&addr.to_hex()).context("converting SuiAddress")
}
