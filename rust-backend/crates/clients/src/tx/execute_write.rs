//! Programmable transaction for `bucket::execute_write`.
//!
//! Unlike admin ops (a single move_call), this PTB stitches together five
//! sub-calls and a SplitCoins so the high-level builder can't help. The
//! sequence:
//!
//! ```text
//! 1. SplitCoins(gas, [write_amount])     -> Coin<Underlying>   (writer flow)
//! 2. coin::zero<Settlement>()            -> Coin<Settlement>   (empty side)
//! 3. quote::new_quote(...)               -> Quote
//! 4. quote::new_signed_quote(q, sig)     -> SignedQuote
//! 5. bucket::writer_flow()               -> FlowKind
//! 6. bucket::execute_write<U,S>(bucket, config, treasury, mm_account,
//!    coin_u, coin_s, flow, position_recipient, call_token_recipient,
//!    signed_quote, clock, ctx)
//! ```
//!
//! **Asset assumption (MVP)**: writer-flow execution assumes
//! `Underlying == 0x2::sui::SUI` so the executor-provided side can be split
//! straight from the gas coin. Non-SUI underlying needs an owned `Coin<U>`
//! object as an extra input — TODO once real test coins land.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use shared_crypto::intent::Intent;
use sui_json_rpc_types::{
    SuiTransactionBlockEffectsAPI, SuiTransactionBlockResponse,
    SuiTransactionBlockResponseOptions,
};
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, Command, ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION, SUI_FRAMEWORK_PACKAGE_ID};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;

pub struct ExecuteWriteParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,

    // Shared objects we mutate or borrow.
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

    // Where the position NFT and the call-option NFT should go.
    pub position_nft_recipient: SuiAddress,
    pub call_token_recipient: SuiAddress,

    pub gas_budget: u64,
}

/// Build, sign, submit a writer-flow `execute_write`. Returns the chain
/// response on success.
pub async fn execute_writer_flow(
    client: &SuiClient,
    signer: &Signer,
    p: &ExecuteWriteParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    let mut pt = ProgrammableTransactionBuilder::new();

    // Shared object args
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let mm_account = pt.obj(shared_object_arg(client, p.mm_account_id, true).await?)?;
    let clock = pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?;

    // Pure inputs
    let arg_protocol_id = pt.pure(&p.protocol_id)?;
    let arg_signer_acct_id = pt.pure(&p.signer_account_id_bytes)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id_bytes)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(&p.signature)?;
    let arg_position_recipient = pt.pure(&p.position_nft_recipient)?;
    let arg_call_token_recipient = pt.pure(&p.call_token_recipient)?;
    let arg_split_amount = pt.pure(&p.write_amount)?;

    // Type tags
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;

    // 1. SplitCoins(gas, [write_amount]) — yields the underlying Coin.
    //    `command` returns `Argument::Result(i)`; SplitCoins's vector of
    //    output coins is addressed via `NestedResult(i, j)`. We pass a
    //    single amount, so the single coin is at j=0.
    let split = pt.command(Command::SplitCoins(
        Argument::GasCoin,
        vec![arg_split_amount],
    ));
    let coin_underlying = nested(split, 0);

    // 2. coin::zero<Settlement>()
    let coin_settlement_zero = pt.programmable_move_call(
        SUI_FRAMEWORK_PACKAGE_ID,
        Identifier::new("coin").unwrap(),
        Identifier::new("zero").unwrap(),
        vec![s_tag.clone()],
        vec![],
    );

    // 3. quote::new_quote(...)
    let quote_val = pt.programmable_move_call(
        p.package,
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

    // 4. quote::new_signed_quote(quote, signature)
    let signed_quote = pt.programmable_move_call(
        p.package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_signed_quote").unwrap(),
        vec![],
        vec![quote_val, arg_signature],
    );

    // 5. bucket::writer_flow()
    let flow = pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("writer_flow").unwrap(),
        vec![],
        vec![],
    );

    // 6. bucket::execute_write<U, S>(...)
    pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("execute_write").unwrap(),
        vec![u_tag, s_tag],
        vec![
            bucket,
            config,
            treasury,
            mm_account,
            coin_underlying,
            coin_settlement_zero,
            flow,
            arg_position_recipient,
            arg_call_token_recipient,
            signed_quote,
            clock,
        ],
    );

    let programmable = pt.finish();

    // Pick a gas coin owned by the executor.
    let gas_coins = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data;
    let gas_coin = gas_coins
        .into_iter()
        .max_by_key(|c| c.balance)
        .ok_or_else(|| anyhow!("no SUI coins to pay gas for {}", signer.address))?;

    let gas_price = client
        .read_api()
        .get_reference_gas_price()
        .await
        .context("fetching reference gas price")?;

    let tx_data = TransactionData::new_programmable(
        signer.address,
        vec![gas_coin.object_ref()],
        programmable,
        p.gas_budget,
        gas_price,
    );

    let sig = Transaction::signature_from_signer(
        tx_data.clone(),
        Intent::sui_transaction(),
        &signer.keypair,
    );
    let tx = Transaction::from_data(tx_data, vec![sig]);
    let opts = SuiTransactionBlockResponseOptions::new()
        .with_effects()
        .with_object_changes();
    let resp = client
        .quorum_driver_api()
        .execute_transaction_block(
            tx,
            opts,
            Some(ExecuteTransactionRequestType::WaitForLocalExecution),
        )
        .await
        .context("submitting execute_write tx")?;
    let effects = resp
        .effects
        .as_ref()
        .context("response missing effects")?;
    if effects.status().is_err() {
        anyhow::bail!("execute_write reverted: {:?}", effects.status());
    }
    Ok(resp)
}

fn nested(parent: Argument, j: u16) -> Argument {
    match parent {
        Argument::Result(i) => Argument::NestedResult(i, j),
        other => panic!(
            "expected Argument::Result from Command, got {:?} (programmer error)",
            other
        ),
    }
}
