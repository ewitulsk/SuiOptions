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
//! package, faucets, coin types) is resolved from the token-info service.
//!
//! ```text
//!   writer --bucket 0x… --write-amount 100000
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
use sui_tx::tx::execute_write::{execute_writer_flow, ExecuteWriteParams, QuoteRouting};
use sui_tx::tx::execute_write_put::{execute_put_writer_flow, ExecutePutWriterParams};
use sui_tx::ws_client;

use writer::{Cli, Product};

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

    // Coin types from the /tokens catalog; the underlying faucet (which the
    // writer mints) from the testTokens passthrough.
    let underlying_spec = snapshot.token_spec(&cli.underlying)?;
    let settlement_spec = snapshot.token_spec(&cli.settlement)?;
    let underlying_faucet = snapshot.faucet_token(&cli.underlying)?;
    let (tokens_pkg, underlying_module) = underlying_faucet.module_path()?;

    let secrets = runtime_config::Secrets::load(&cli.secrets)
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
    // The collateral routing is read straight off the SIGNED quote — the
    // PTB targets `{release_package}::{release_module}::release` directly.
    let quote_signer_id = sui_object_id_from_pt(best.quote.signer_id)?;
    let collateral_account_id = sui_object_id_from_pt(best.quote.collateral_source)?;
    let release_package =
        ObjectID::from_str(&best.quote.release_package.to_hex()).context("release_package")?;
    let signer_token_recipient =
        sui_address_from_pt(best.quote.signer_token_recipient)?;

    // The option coin type is the bucket's third type parameter; read it off
    // the on-chain Bucket<U, S, Call|Put> object (index 2 holds either).
    let option_type = bucket_call_type(&wrap.client, cli.bucket).await?;

    let resp = match cli.product {
        Product::Call => {
            let params = ExecuteWriteParams {
                package,
                underlying_type: &underlying_spec.coin_type,
                settlement_type: &settlement_spec.coin_type,
                call_type: &option_type,
                tokens_package: tokens_pkg,
                underlying_module: &underlying_module,
                underlying_faucet_id: underlying_faucet.faucet()?,
                bucket_id: cli.bucket,
                protocol_config_id: protocol_config,
                treasury_id: treasury,
                routing: QuoteRouting {
                    quote_signer_id,
                    collateral_account_id,
                    release_package,
                    release_module: &best.quote.release_module,
                },
                protocol_id: best.quote.protocol_id.clone(),
                signer_token_recipient,
                write_amount: best.quote.write_amount,
                premium: best.quote.premium,
                valid_until_ms: best.quote.valid_until_ms,
                nonce: best.quote.nonce,
                signature: best.signature.clone(),
                position_recipient: writer_addr,
                gas_budget: cli.gas_budget,
            };
            execute_writer_flow(&wrap.client, &wrap.signer, &params).await?
        }
        Product::Put => {
            // Puts settle in cash: the writer mints SETTLEMENT collateral
            // (not underlying), so resolve the settlement faucet/module here.
            let settlement_faucet = snapshot.faucet_token(&cli.settlement)?;
            let (settlement_tokens_pkg, settlement_module) = settlement_faucet.module_path()?;
            let strike = cli
                .strike
                .ok_or_else(|| anyhow!("--strike is required for --product put"))?;
            let collateral = put_collateral(best.quote.write_amount, strike, cli.strike_scale)?;
            let params = ExecutePutWriterParams {
                package,
                underlying_type: &underlying_spec.coin_type,
                settlement_type: &settlement_spec.coin_type,
                put_type: &option_type,
                tokens_package: settlement_tokens_pkg,
                settlement_module: &settlement_module,
                settlement_faucet_id: settlement_faucet.faucet()?,
                bucket_id: cli.bucket,
                protocol_config_id: protocol_config,
                treasury_id: treasury,
                routing: QuoteRouting {
                    quote_signer_id,
                    collateral_account_id,
                    release_package,
                    release_module: &best.quote.release_module,
                },
                protocol_id: best.quote.protocol_id.clone(),
                signer_token_recipient,
                write_amount: best.quote.write_amount,
                premium: best.quote.premium,
                valid_until_ms: best.quote.valid_until_ms,
                nonce: best.quote.nonce,
                signature: best.signature.clone(),
                collateral,
                position_recipient: writer_addr,
                gas_budget: cli.gas_budget,
            };
            execute_put_writer_flow(&wrap.client, &wrap.signer, &params).await?
        }
    };
    println!("✓ execute_write digest: {}", sui_tx::tx::tx_digest(&resp));
    Ok(())
}

/// Cash collateral for a written put = ceil(write_amount × strike / 10^scale),
/// in settlement smallest-units. Saturates rather than panicking on overflow.
fn put_collateral(write_amount: u64, strike: u128, strike_scale: u8) -> Result<u64> {
    let scale = 10u128
        .checked_pow(strike_scale as u32)
        .ok_or_else(|| anyhow!("strike_scale {strike_scale} too large"))?;
    let numerator = (write_amount as u128)
        .checked_mul(strike)
        .ok_or_else(|| anyhow!("write_amount × strike overflows u128"))?;
    let collateral = numerator.div_ceil(scale);
    u64::try_from(collateral)
        .map_err(|_| anyhow!("collateral {collateral} exceeds u64 range"))
}

/// Read the bucket's option-coin type (the `Call` in `Bucket<U, S, Call>`)
/// straight off the on-chain object's type parameters.
async fn bucket_call_type(client: &sui_tx::chain::ChainClient, bucket: ObjectID) -> Result<String> {
    let tag = client
        .get_object(bucket)
        .await?
        .struct_tag()
        .ok_or_else(|| anyhow!("bucket {bucket} has no struct type"))?;
    let call = tag
        .type_params
        .get(2)
        .ok_or_else(|| anyhow!("bucket type has no Call type param: {tag}"))?;
    Ok(call.to_canonical_string(/* with_prefix */ true))
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
