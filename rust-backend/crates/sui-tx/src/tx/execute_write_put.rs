//! Programmable transactions for the cash-secured-put collateral protocol —
//! the mirror of [`super::execute_write`].
//!
//! For puts BOTH legs are `Coin<Settlement>`/`Balance<Settlement>` (the
//! collateral and the premium are cash), so both flows mint from the
//! SETTLEMENT faucet and both requests release `Settlement`:
//!
//! - Writer flow: executor mints `collateral = ceil(write_amount × strike)`
//!   settlement; the MM (signer / put buyer) pays the premium via its
//!   `release` implementation.
//! - Trader flow: executor mints the `premium` in settlement; the MM
//!   (signer / put writer) posts the cash collateral via `release`.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, SharedObjectMutability};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::info;

use protocol_types::bucket_spec::BucketSpec;

use crate::sui_client::Signer;
use crate::tx::execute_write::{build_request_and_release, FlowPrelude, QuoteRouting};
use crate::tx::{shared_object_arg, submit_ptb};
use crate::chain::{ChainClient, ExecutedTransaction};

/// Inputs for the put writer-flow PTB: the executor (retail put writer) posts
/// cash collateral; the signer MM buys the put.
pub struct ExecutePutWriterParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    /// Fully-qualified per-bucket put coin type (`0x<gen_pkg>::put_<i>::PUT_<I>`).
    pub put_type: &'a str,

    /// Test-tokens package id holding the faucets.
    pub tokens_package: ObjectID,
    /// Lowercase module name of the *settlement* test-token, e.g. `"tusdc"`.
    pub settlement_module: &'a str,
    /// Shared `Faucet` object id for the settlement coin.
    pub settlement_faucet_id: ObjectID,

    // Shared protocol objects.
    pub bucket_id: ObjectID,
    pub protocol_config_id: ObjectID,
    /// Shared `whitelist::Whitelist` — the ingress gate the execute
    /// entries take between the config and the treasury (SO-382).
    pub whitelist_id: ObjectID,
    pub treasury_id: ObjectID,

    /// Signer + collateral routing, all derived from the signed quote.
    pub routing: QuoteRouting<'a>,

    // Quote fields the MM signed over (BCS-canonical).
    pub protocol_id: Vec<u8>,
    pub signer_token_recipient: SuiAddress,
    /// The bucket's economics — what the MM signed. `bucket_id` above is the
    /// object the PTB references; this is the agreement it is checked against.
    pub spec: BucketSpec,
    /// Signed queue bound; `u128::MAX` opts out.
    pub max_total_written: u128,
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Cash collateral to mint and escrow = ceil(write_amount × strike).
    pub collateral: u64,

    /// Writer flow: the executor (put writer) receives the Position. The put
    /// coin goes to the quote's `signer_token_recipient` on chain.
    pub position_recipient: SuiAddress,

    pub gas_budget: u64,
}

/// Inputs for the put trader-flow PTB: the executor (retail put buyer) pays the
/// premium; the signer MM writes the put (posts cash collateral).
pub struct ExecutePutTraderParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub put_type: &'a str,

    pub tokens_package: ObjectID,
    pub settlement_module: &'a str,
    pub settlement_faucet_id: ObjectID,

    pub bucket_id: ObjectID,
    pub protocol_config_id: ObjectID,
    /// Shared `whitelist::Whitelist` — the ingress gate the execute
    /// entries take between the config and the treasury (SO-382).
    pub whitelist_id: ObjectID,
    pub treasury_id: ObjectID,

    /// Signer + collateral routing, all derived from the signed quote.
    pub routing: QuoteRouting<'a>,

    pub protocol_id: Vec<u8>,
    pub signer_token_recipient: SuiAddress,
    /// The bucket's economics — what the MM signed. `bucket_id` above is the
    /// object the PTB references; this is the agreement it is checked against.
    pub spec: BucketSpec,
    /// Signed queue bound; `u128::MAX` opts out.
    pub max_total_written: u128,
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Trader flow: the retail trader receives the put coins. The Position
    /// goes to the quote's `signer_token_recipient` on chain.
    pub put_token_recipient: SuiAddress,

    pub gas_budget: u64,
}

fn clock_arg(pt: &mut ProgrammableTransactionBuilder) -> Result<sui_types::transaction::Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

/// Build + sign + submit the put writer-flow PTB.
pub async fn execute_put_writer_flow(
    client: &ChainClient,
    signer: &Signer,
    p: &ExecutePutWriterParams<'_>,
) -> Result<ExecutedTransaction> {
    info!(
        %p.package, %p.bucket_id,
        write_amount = p.write_amount, premium = p.premium,
        collateral = p.collateral, nonce = p.nonce,
        release_package = %p.routing.release_package,
        "building put execute_writer_flow PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, p.whitelist_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_position_recipient = pt.pure(&p.position_recipient)?;
    let arg_collateral_amount = pt.pure(&p.collateral)?;

    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let put_tag = TypeTag::from_str(p.put_type)
        .with_context(|| format!("parsing put type {}", p.put_type))?;

    // 1. mint the cash collateral (executor side).
    let coin_collateral = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_collateral_amount],
    );

    // 2–5. quote → signed quote → request (premium demanded in Settlement)
    // → release<Settlement>.
    let (request, funds) = build_request_and_release(
        client,
        &mut pt,
        FlowPrelude {
            package: p.package,
            request_module: "put_bucket",
            request_function: "request_writer_flow",
            request_type_args: vec![u_tag.clone(), s_tag.clone(), put_tag.clone()],
            release_type: s_tag.clone(),
            routing: &p.routing,
            protocol_id: &p.protocol_id,
            signer_token_recipient: p.signer_token_recipient,
            spec: &p.spec,
            max_total_written: p.max_total_written,
            write_amount: p.write_amount,
            premium: p.premium,
            valid_until_ms: p.valid_until_ms,
            nonce: p.nonce,
            signature: &p.signature,
        },
        bucket,
        config,
        clock,
    )
    .await?;

    // 6. put_bucket::execute_writer_flow<U, S, Put>(...)
    pt.programmable_move_call(
        p.package,
        Identifier::new("put_bucket").unwrap(),
        Identifier::new("execute_writer_flow").unwrap(),
        vec![u_tag, s_tag, put_tag],
        vec![
            bucket, config, wl, treasury, request, funds, coin_collateral,
            arg_position_recipient, clock,
        ],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "put_bucket::execute_writer_flow").await
}

/// Build + sign + submit the put trader-flow PTB.
pub async fn execute_put_trader_flow(
    client: &ChainClient,
    signer: &Signer,
    p: &ExecutePutTraderParams<'_>,
) -> Result<ExecutedTransaction> {
    info!(
        %p.package, %p.bucket_id,
        write_amount = p.write_amount, premium = p.premium, nonce = p.nonce,
        release_package = %p.routing.release_package,
        "building put execute_trader_flow PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, p.whitelist_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_put_token_recipient = pt.pure(&p.put_token_recipient)?;
    let arg_mint_amount = pt.pure(&p.premium)?;

    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let put_tag = TypeTag::from_str(p.put_type)
        .with_context(|| format!("parsing put type {}", p.put_type))?;

    // 1. mint the premium (executor side).
    let coin_premium = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_mint_amount],
    );

    // 2–5. quote → signed quote → request (cash collateral demanded)
    // → release<Settlement>.
    let (request, funds) = build_request_and_release(
        client,
        &mut pt,
        FlowPrelude {
            package: p.package,
            request_module: "put_bucket",
            request_function: "request_trader_flow",
            request_type_args: vec![u_tag.clone(), s_tag.clone(), put_tag.clone()],
            release_type: s_tag.clone(),
            routing: &p.routing,
            protocol_id: &p.protocol_id,
            signer_token_recipient: p.signer_token_recipient,
            spec: &p.spec,
            max_total_written: p.max_total_written,
            write_amount: p.write_amount,
            premium: p.premium,
            valid_until_ms: p.valid_until_ms,
            nonce: p.nonce,
            signature: &p.signature,
        },
        bucket,
        config,
        clock,
    )
    .await?;

    // 6. put_bucket::execute_trader_flow<U, S, Put>(...)
    pt.programmable_move_call(
        p.package,
        Identifier::new("put_bucket").unwrap(),
        Identifier::new("execute_trader_flow").unwrap(),
        vec![u_tag, s_tag, put_tag],
        vec![
            bucket, config, wl, treasury, request, funds, coin_premium,
            arg_put_token_recipient, clock,
        ],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "put_bucket::execute_trader_flow").await
}
