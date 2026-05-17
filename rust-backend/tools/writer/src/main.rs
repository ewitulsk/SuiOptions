//! Retail writer CLI — stands in for the off-chain frontend.
//!
//! End-to-end §8.1 writer flow:
//!
//! 1. Connect to the quoting service WS as `RetailRole::Writer`.
//! 2. `RFQRequest` for `(bucket_id, write_amount, side=Writer)`.
//! 3. Pick the top quote (service already sorted by best premium).
//! 4. Submit a writer-flow `execute_write` PTB. The PTB itself mints the
//!    underlying via the configured test-token faucet (no pre-mint step),
//!    so a single tx covers the whole flow.
//!
//! Every on-chain id (package, ProtocolConfig, Treasury, test-tokens
//! package, faucets, coin types) is resolved from `deployments.json`.
//!
//! ```text
//!   writer --bucket 0x… --write-amount 100000
//!          --underlying TBTC --settlement TUSDC
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use sui_types::base_types::ObjectID;

use shared::protocol_types::ids::ObjectId as PtObjectId;
use shared::protocol_types::messages::{
    RetailHelloPayload, RetailToService, RfqRequestPayload, ServiceToRetail,
};
use shared::protocol_types::sides::{RetailRole, Side};

use shared::deployments::Deployments;
use shared::sui_client::{Network, SuiClientWrapper};
use shared::tx::execute_write::{execute_writer_flow, ExecuteWriteParams};
use shared::ws_client;

#[derive(Parser)]
#[command(name = "writer", about = "Retail-writer test client for the options protocol")]
struct Cli {
    #[arg(short, long, default_value = "deployments.json")]
    deployments: PathBuf,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/writer/config/secrets.toml")]
    secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    #[arg(short = 'q', long, default_value = "ws://127.0.0.1:9002/")]
    quoting_url: String,

    /// Bucket id we're writing into.
    #[arg(short, long)]
    bucket: ObjectID,

    /// Underlying amount we're writing, in raw smallest-units (see token
    /// decimals in `deployments.json::testTokens`).
    #[arg(short = 'w', long)]
    write_amount: u64,

    /// Symbol for the underlying token (TBTC, TDEEP, TUSDC, TWAL).
    #[arg(long, default_value = "TBTC")]
    underlying: String,

    /// Symbol for the settlement token.
    #[arg(long, default_value = "TUSDC")]
    settlement: String,

    #[arg(long, default_value_t = 200_000_000)]
    gas_budget: u64,

    #[arg(long, default_value_t = 5)]
    rfq_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let dep = Deployments::load(&cli.deployments)
        .with_context(|| format!("loading {}", cli.deployments.display()))?;
    let net = dep.for_network(cli.network.as_str())?;

    let package = net.package()?;
    let protocol_config = net.protocol_config()?;
    let treasury = net.treasury().context("treasury_id missing from deployments")?;

    // Resolve underlying + settlement via testTokens.
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
    let signer_token_recipient =
        sui_address_from_pt(best.quote.signer_token_recipient)?;

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
    println!("✓ execute_write digest: {}", resp.digest);
    Ok(())
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
