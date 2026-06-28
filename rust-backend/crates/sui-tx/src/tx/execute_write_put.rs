//! Programmable transactions for `put_bucket::execute_write` (cash-secured
//! puts) — the mirror of [`super::execute_write`].
//!
//! For puts BOTH coin legs are `Coin<Settlement>` (the collateral and the
//! premium are cash), so both flows mint from the SETTLEMENT faucet:
//!
//! - Writer flow: executor mints `collateral = ceil(write_amount × strike)`
//!   settlement, premium side is `coin::zero<Settlement>`; the MM (signer /
//!   put buyer) pays the premium from their Account.
//! - Trader flow: executor mints the `premium` in settlement, collateral side
//!   is `coin::zero<Settlement>`; the MM (signer / put writer) posts the cash
//!   collateral from their Account.
//!
//! The `FlowKind` marker is the shared `bucket::writer_flow()` /
//! `bucket::trader_flow()` (put_bucket reuses `bucket::FlowKind`).

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{ObjectArg, SharedObjectMutability};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION, SUI_FRAMEWORK_PACKAGE_ID};
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::{shared_object_arg, submit_ptb};

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
    pub treasury_id: ObjectID,
    pub mm_account_id: ObjectID,

    // Quote fields the MM signed over (BCS-canonical).
    pub protocol_id: Vec<u8>,
    pub signer_account_id_bytes: [u8; 32],
    pub signer_token_recipient: SuiAddress,
    pub bucket_id_bytes: [u8; 32],
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Cash collateral to mint and escrow = ceil(write_amount × strike).
    pub collateral: u64,

    /// Writer flow: the executor (put writer) receives the Position.
    pub position_recipient: SuiAddress,
    /// Writer flow: must equal `signer_token_recipient` (the MM gets the puts).
    pub put_token_recipient: SuiAddress,

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
    pub treasury_id: ObjectID,
    pub mm_account_id: ObjectID,

    pub protocol_id: Vec<u8>,
    pub signer_account_id_bytes: [u8; 32],
    pub signer_token_recipient: SuiAddress,
    pub bucket_id_bytes: [u8; 32],
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Trader flow: must equal `signer_token_recipient` (the MM gets the Position).
    pub position_recipient: SuiAddress,
    /// Trader flow: the retail trader receives the put coins.
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
    client: &SuiClient,
    signer: &Signer,
    p: &ExecutePutWriterParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(
        %p.package, %p.bucket_id,
        write_amount = p.write_amount, premium = p.premium,
        collateral = p.collateral, nonce = p.nonce,
        "building put execute_write (writer flow) PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let mm_account = pt.obj(shared_object_arg(client, p.mm_account_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_protocol_id = pt.pure(&p.protocol_id)?;
    let arg_signer_acct_id = pt.pure(&p.signer_account_id_bytes)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id_bytes)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(&p.signature)?;
    let arg_position_recipient = pt.pure(&p.position_recipient)?;
    let arg_put_token_recipient = pt.pure(&p.put_token_recipient)?;
    let arg_collateral_amount = pt.pure(&p.collateral)?;

    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let put_tag = TypeTag::from_str(p.put_type)
        .with_context(|| format!("parsing put type {}", p.put_type))?;

    // 1. mint collateral settlement.
    let coin_collateral = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_collateral_amount],
    );
    // 2. coin::zero<Settlement>() — premium comes from the MM's Account.
    let coin_premium_zero = pt.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("coin").unwrap(),
        Identifier::new("zero").unwrap(),
        vec![s_tag.clone()],
        vec![],
    );
    let signed_quote = build_signed_quote(
        &mut pt, p.package, arg_protocol_id, arg_signer_acct_id, arg_signer_token_recipient,
        arg_bucket_id, arg_write_amount, arg_premium, arg_valid_until_ms, arg_nonce, arg_signature,
    );
    let flow = pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("writer_flow").unwrap(),
        vec![],
        vec![],
    );
    pt.programmable_move_call(
        p.package,
        Identifier::new("put_bucket").unwrap(),
        Identifier::new("execute_write").unwrap(),
        vec![u_tag, s_tag, put_tag],
        vec![
            bucket, config, treasury, mm_account,
            coin_collateral, coin_premium_zero, flow,
            arg_position_recipient, arg_put_token_recipient, signed_quote, clock,
        ],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "put_bucket::execute_write (writer)").await
}

/// Build + sign + submit the put trader-flow PTB.
pub async fn execute_put_trader_flow(
    client: &SuiClient,
    signer: &Signer,
    p: &ExecutePutTraderParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(
        %p.package, %p.bucket_id,
        write_amount = p.write_amount, premium = p.premium, nonce = p.nonce,
        "building put execute_write (trader flow) PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let mm_account = pt.obj(shared_object_arg(client, p.mm_account_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_protocol_id = pt.pure(&p.protocol_id)?;
    let arg_signer_acct_id = pt.pure(&p.signer_account_id_bytes)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id_bytes)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(&p.signature)?;
    let arg_position_recipient = pt.pure(&p.position_recipient)?;
    let arg_put_token_recipient = pt.pure(&p.put_token_recipient)?;
    let arg_mint_amount = pt.pure(&p.premium)?;

    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let put_tag = TypeTag::from_str(p.put_type)
        .with_context(|| format!("parsing put type {}", p.put_type))?;

    // 1. coin::zero<Settlement>() — collateral comes from the MM's Account.
    let coin_collateral_zero = pt.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("coin").unwrap(),
        Identifier::new("zero").unwrap(),
        vec![s_tag.clone()],
        vec![],
    );
    // 2. mint premium settlement.
    let coin_premium = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_mint_amount],
    );
    let signed_quote = build_signed_quote(
        &mut pt, p.package, arg_protocol_id, arg_signer_acct_id, arg_signer_token_recipient,
        arg_bucket_id, arg_write_amount, arg_premium, arg_valid_until_ms, arg_nonce, arg_signature,
    );
    let flow = pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("trader_flow").unwrap(),
        vec![],
        vec![],
    );
    pt.programmable_move_call(
        p.package,
        Identifier::new("put_bucket").unwrap(),
        Identifier::new("execute_write").unwrap(),
        vec![u_tag, s_tag, put_tag],
        vec![
            bucket, config, treasury, mm_account,
            coin_collateral_zero, coin_premium, flow,
            arg_position_recipient, arg_put_token_recipient, signed_quote, clock,
        ],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "put_bucket::execute_write (trader)").await
}

/// Shared `quote::new_quote` → `quote::new_signed_quote` prelude.
#[allow(clippy::too_many_arguments)]
fn build_signed_quote(
    pt: &mut ProgrammableTransactionBuilder,
    package: ObjectID,
    arg_protocol_id: sui_types::transaction::Argument,
    arg_signer_acct_id: sui_types::transaction::Argument,
    arg_signer_token_recipient: sui_types::transaction::Argument,
    arg_bucket_id: sui_types::transaction::Argument,
    arg_write_amount: sui_types::transaction::Argument,
    arg_premium: sui_types::transaction::Argument,
    arg_valid_until_ms: sui_types::transaction::Argument,
    arg_nonce: sui_types::transaction::Argument,
    arg_signature: sui_types::transaction::Argument,
) -> sui_types::transaction::Argument {
    let quote_val = pt.programmable_move_call(
        package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_quote").unwrap(),
        vec![],
        vec![
            arg_protocol_id,
            arg_signer_acct_id,
            arg_signer_token_recipient,
            arg_bucket_id,
            arg_write_amount,
            arg_premium,
            arg_valid_until_ms,
            arg_nonce,
        ],
    );
    pt.programmable_move_call(
        package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_signed_quote").unwrap(),
        vec![],
        vec![quote_val, arg_signature],
    )
}
