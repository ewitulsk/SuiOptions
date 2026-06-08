//! Retail trader CLI — stands in for the off-chain frontend's Buy page.
//!
//! End-to-end §7.2 trader flow:
//!
//! 1. Connect to the quoting service WS as `RetailRole::Trader`.
//! 2. `RFQRequest` for `(bucket_id, write_amount, side=Trader)`.
//! 3. Pick the top quote (service already sorted asks lowest-premium-first).
//! 4. Submit a trader-flow `execute_write` PTB. The PTB itself mints the
//!    settlement premium via the configured test-token faucet (no pre-mint
//!    step), so a single tx covers the whole flow. The Writer MM provides the
//!    underlying from their Account and receives the Position NFT; this trader
//!    receives the CallOption coin.
//!
//! Every on-chain id (package, ProtocolConfig, Treasury, test-tokens
//! package, faucets, coin types) is resolved from the token-info service.
//!
//! ```text
//!   trader --bucket 0x… --write-amount 100000
//!          --underlying TBTC --settlement TUSDC
//! ```

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

use token_info_client::TokenInfoClient;
use sui_tx::sui_client::SuiClientWrapper;
use sui_tx::tx::execute_write::{execute_trader_flow, ExecuteTraderParams};
use sui_tx::ws_client;

use trader::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let snapshot = TokenInfoClient::new(&cli.token_info_url)
        .fetch_blocking_until_ready(30, std::time::Duration::from_secs(2))
        .await?;

    let package = snapshot.package()?;
    let protocol_config = snapshot.protocol_config()?;
    let treasury = snapshot.treasury().context("treasury_id missing from token-info")?;

    // Coin types from the /tokens catalog; the settlement faucet (which the
    // trader mints to pay the premium) from the testTokens passthrough.
    let underlying_spec = snapshot.token_spec(&cli.underlying)?;
    let settlement_spec = snapshot.token_spec(&cli.settlement)?;
    let settlement_faucet = snapshot.faucet_token(&cli.settlement)?;
    let (tokens_pkg, settlement_module) = settlement_faucet.module_path()?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
        .with_context(|| format!("loading secrets {}", cli.secrets.display()))?;
    let wrap = SuiClientWrapper::connect(&secrets, cli.network).await?;
    let trader_addr = wrap.signer.address;
    tracing::info!(%trader_addr, underlying = %cli.underlying, settlement = %cli.settlement, "trader ready");

    // -- WS handshake -----------------------------------------------------
    let mut ws = ws_client::connect(&cli.quoting_url).await?;
    ws_client::send_json(
        &mut ws,
        &RetailToService::Hello {
            payload: RetailHelloPayload {
                role: RetailRole::Trader,
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
                side: Side::Trader,
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
        "selected best ask"
    );

    // -- Execute write (trader flow) -------------------------------------
    let mm_account_id = sui_object_id_from_pt(best.mm_id)?;
    let bucket_id_bytes: [u8; 32] = *best.quote.bucket_id.as_bytes();
    let signer_account_id_bytes: [u8; 32] = *best.quote.signer_account_id.as_bytes();
    let signer_token_recipient = sui_address_from_pt(best.quote.signer_token_recipient)?;

    // The option coin type is the bucket's third type parameter; read it off
    // the on-chain Bucket<U, S, Call> object.
    let call_type = bucket_call_type(&wrap.client, cli.bucket).await?;

    let params = ExecuteTraderParams {
        package,
        underlying_type: &underlying_spec.coin_type,
        settlement_type: &settlement_spec.coin_type,
        call_type: &call_type,
        tokens_package: tokens_pkg,
        settlement_module: &settlement_module,
        settlement_faucet_id: settlement_faucet.faucet()?,
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
        // Trader flow requires position_recipient == signer_token_recipient (the
        // MM gets the Position NFT); the trader receives the CallOption.
        position_recipient: signer_token_recipient,
        call_token_recipient: trader_addr,
        gas_budget: cli.gas_budget,
    };

    let resp = execute_trader_flow(&wrap.client, &wrap.signer, &params).await?;
    println!("✓ execute_write (trader) digest: {}", resp.digest);
    Ok(())
}

/// Read the bucket's option-coin type (the `Call` in `Bucket<U, S, Call>`)
/// straight off the on-chain object's type parameters.
async fn bucket_call_type(client: &sui_sdk::SuiClient, bucket: ObjectID) -> Result<String> {
    let resp = client
        .read_api()
        .get_object_with_options(
            bucket,
            sui_json_rpc_types::SuiObjectDataOptions::new().with_type(),
        )
        .await?;
    let data = resp.data.ok_or_else(|| anyhow!("bucket {bucket} not found on chain"))?;
    let object_type = data.object_type().context("bucket object has no type")?;
    match object_type {
        sui_types::base_types::ObjectType::Struct(mot) => {
            let params = mot.type_params();
            let call = params
                .get(2)
                .ok_or_else(|| anyhow!("bucket type has no Call type param: {mot}"))?;
            Ok(call.to_canonical_string(/* with_prefix */ true))
        }
        other => Err(anyhow!("bucket object is not a struct type: {other}")),
    }
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
    addr: protocol_types::ids::SuiAddress,
) -> Result<sui_types::base_types::SuiAddress> {
    sui_types::base_types::SuiAddress::from_str(&addr.to_hex()).context("converting SuiAddress")
}
