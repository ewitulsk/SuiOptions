//! Retail writer CLI — the off-chain frontend stand-in.
//!
//! Walks through the full §8.1 writer flow:
//!
//! 1. Connect to the quoting service WS as `RetailRole::Writer`.
//! 2. `RFQRequest` for `(bucket_id, write_amount, side=Writer)`.
//! 3. Pick the top quote (the service already sorted by best premium).
//! 4. Build + sign + submit a writer-flow `execute_write` PTB. The PTB
//!    splits `write_amount` of the underlying from gas (MVP assumes
//!    `Underlying == 0x2::sui::SUI`), splices in the MM's signed Quote,
//!    and lands the position NFT in our wallet.
//!
//! ```text
//!   writer --quoting-url ws://127.0.0.1:9002/ \
//!          --bucket 0x… --write-amount 10000000 \
//!          --underlying 0x2::sui::SUI --settlement 0x2::sui::SUI
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use sui_types::base_types::ObjectID;

use protocol_types::ids::ObjectId as PtObjectId;
use protocol_types::messages::{
    RetailHelloPayload, RetailToService, RfqRequestPayload, ServiceToRetail,
};
use protocol_types::sides::{RetailRole, Side};

use clients::deployments::Deployments;
use clients::sui_client::{Network, SuiClientWrapper};
use clients::tx::execute_write::{execute_writer_flow, ExecuteWriteParams};
use clients::ws_client;

#[derive(Parser)]
#[command(name = "writer", about = "Retail-writer test client for the options protocol")]
struct Cli {
    /// Path to deployments.json.
    #[arg(short, long, default_value = "deployments.json")]
    deployments: PathBuf,

    /// Target network.
    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    network: Network,

    /// Quoting-service WS endpoint.
    #[arg(short = 'q', long, default_value = "ws://127.0.0.1:9002/")]
    quoting_url: String,

    /// Bucket id we're writing into.
    #[arg(short, long)]
    bucket: ObjectID,

    /// Underlying amount we're writing, in raw smallest-units.
    #[arg(short = 'w', long)]
    write_amount: u64,

    /// Underlying Move type. MVP requires `0x2::sui::SUI` so we can split
    /// from gas; flag is exposed for forward compatibility.
    #[arg(long, default_value = "0x2::sui::SUI")]
    underlying: String,

    /// Settlement Move type.
    #[arg(long, default_value = "0x2::sui::SUI")]
    settlement: String,

    /// Gas budget per PTB (MIST).
    #[arg(long, default_value_t = 200_000_000)]
    gas_budget: u64,

    /// How long to wait for the service's RFQResponse.
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
    let net = dep.for_network(cli.network)?;
    let package = net.package()?;
    let protocol_config = net.protocol_config()?;
    let treasury = net.treasury().context("treasury_id missing from deployments")?;

    let wrap = SuiClientWrapper::connect(cli.network).await?;
    let writer_addr = wrap.signer.address;
    tracing::info!(%writer_addr, "writer signer ready");

    // -- 1. Connect to quoting service ------------------------------------
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

    // -- 2. RFQ -----------------------------------------------------------
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

    // -- 3. Execute write -------------------------------------------------
    let mm_account_id = sui_object_id_from_pt(best.mm_id)?;
    let bucket_id_bytes: [u8; 32] = *best.quote.bucket_id.as_bytes();
    let signer_account_id_bytes: [u8; 32] = *best.quote.signer_account_id.as_bytes();
    let signer_token_recipient =
        sui_address_from_pt(best.quote.signer_token_recipient)?;

    let params = ExecuteWriteParams {
        package,
        underlying_type: &cli.underlying,
        settlement_type: &cli.settlement,
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
        // Writer flow requires signer_token_recipient == call_token_recipient,
        // so we pass the quote's value through.
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

fn sui_address_from_pt(addr: protocol_types::ids::SuiAddress) -> Result<sui_types::base_types::SuiAddress> {
    sui_types::base_types::SuiAddress::from_str(&addr.to_hex()).context("converting SuiAddress")
}
