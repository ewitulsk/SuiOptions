//! Programmable transactions for the split `bucket::execute_write_*` entry
//! points (org-aware protocol).
//!
//! Writer flow — single PTB, four commands:
//!
//! ```text
//! 1. test_tokens::<u_module>::mint(faucet, write_amount)  -> Coin<Underlying>
//! 2. quote::new_quote(...)                                -> Quote
//! 3. quote::new_signed_quote(q, sig)                      -> SignedQuote
//! 4. bucket::execute_write_writer_flow<U, S, Call>(bucket, org, config,
//!    treasury, mm_account, coin_u, signed_quote, clock, ctx)
//!                                              -> (Position, Coin<Settlement>)
//! 5. TransferObjects([result.0, result.1], executor_recipient)
//! ```
//!
//! The contract returns the executor's side to the PTB (the writer's
//! Position + net premium here; the trader's Coin<Call> in trader flow) and
//! the trailing `TransferObjects` routes it; the signer/MM's side is still
//! transferred by the contract to the quote's `signer_token_recipient`.
//!
//! The faucet mint composes with the rest of the PTB because the test-token
//! `mint(&mut Faucet, u64, ctx): Coin<T>` is a non-entry public function —
//! its result is addressable as a PTB `Argument`.
//!
//! NOTE: the frontend builds the same PTB shapes in
//! `frontend/src/tx/composer.ts`, and the gas-station validates them against
//! the templates in `template.rs` — keep all three in sync
//! (see `.claude/ptb-sync.md`).

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
    Argument, ObjectArg, SharedObjectMutability, Transaction, TransactionData,
};
use sui_types::transaction_driver_types::ExecuteTransactionRequestType;
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;

pub struct ExecuteWriteParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    /// Fully-qualified type of the bucket's per-bucket option coin
    /// (`0x<gen_pkg>::call_<i>::CALL_<I>`).
    pub call_type: &'a str,

    /// Test-tokens package id holding the faucets.
    pub tokens_package: ObjectID,
    /// Lowercase module name in the test-tokens package, e.g. `"tbtc"`.
    pub underlying_module: &'a str,
    /// Shared `Faucet` object id for the underlying coin.
    pub underlying_faucet_id: ObjectID,

    // Shared protocol objects.
    pub bucket_id: ObjectID,
    /// The bucket's Org (shared, mutated for the org fee deposit).
    pub org_id: ObjectID,
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

    /// Where the PTB transfers the RETURNED (Position, net-premium Coin) —
    /// conventionally the executor (the writer). The MM's Coin<Call> is
    /// contract-routed to `signer_token_recipient` and is not configurable.
    pub executor_recipient: SuiAddress,

    pub gas_budget: u64,
}

/// Inputs for the trader-flow PTB. Mirrors [`ExecuteWriteParams`] but the
/// executor mints the *settlement* premium (not underlying), so the faucet
/// fields point at the settlement test-token instead.
pub struct ExecuteTraderParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    /// Fully-qualified type of the bucket's per-bucket option coin.
    pub call_type: &'a str,

    /// Test-tokens package id holding the faucets.
    pub tokens_package: ObjectID,
    /// Lowercase module name of the *settlement* test-token, e.g. `"tusdc"`.
    pub settlement_module: &'a str,
    /// Shared `Faucet` object id for the settlement coin.
    pub settlement_faucet_id: ObjectID,

    // Shared protocol objects.
    pub bucket_id: ObjectID,
    /// The bucket's Org (shared, mutated for the org fee deposit).
    pub org_id: ObjectID,
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

    /// Where the PTB transfers the RETURNED Coin<Call> — conventionally the
    /// executor (the trader). The MM's Position is contract-routed to
    /// `signer_token_recipient` and is not configurable.
    pub executor_recipient: SuiAddress,

    pub gas_budget: u64,
}

/// Build + sign + submit the writer-flow PTB.
pub async fn execute_writer_flow(
    client: &SuiClient,
    signer: &Signer,
    p: &ExecuteWriteParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(
        %p.package,
        %p.bucket_id,
        write_amount = p.write_amount,
        premium = p.premium,
        nonce = p.nonce,
        "building execute_write PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // Shared object args.
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let org = pt.obj(shared_object_arg(client, p.org_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let mm_account = pt.obj(shared_object_arg(client, p.mm_account_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.underlying_faucet_id, true).await?)?;
    let clock = pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?;

    // Pure inputs.
    let arg_protocol_id = pt.pure(&p.protocol_id)?;
    let arg_signer_acct_id = pt.pure(&p.signer_account_id_bytes)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id_bytes)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(&p.signature)?;
    let arg_mint_amount = pt.pure(&p.write_amount)?;

    // Type tags.
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let c_tag = TypeTag::from_str(p.call_type)
        .with_context(|| format!("parsing call type {}", p.call_type))?;

    // 1. test_tokens::<module>::mint(faucet, write_amount) -> Coin<Underlying>
    let coin_underlying = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.underlying_module)
            .map_err(|e| anyhow!("underlying module {}: {e}", p.underlying_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_mint_amount],
    );

    // 2. quote::new_quote(...)
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

    // 3. quote::new_signed_quote(quote, signature)
    let signed_quote = pt.programmable_move_call(
        p.package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_signed_quote").unwrap(),
        vec![],
        vec![quote_val, arg_signature],
    );

    // 4. bucket::execute_write_writer_flow<U, S, Call>(...)
    //    -> (Position, Coin<Settlement>) returned to the PTB.
    let result = pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("execute_write_writer_flow").unwrap(),
        vec![u_tag, s_tag, c_tag],
        vec![
            bucket,
            org,
            config,
            treasury,
            mm_account,
            coin_underlying,
            signed_quote,
            clock,
        ],
    );
    let Argument::Result(idx) = result else {
        return Err(anyhow!("unexpected non-Result argument from move call"));
    };

    // 5. Route the returned (Position, net premium) to the executor.
    pt.transfer_args(
        p.executor_recipient,
        vec![Argument::NestedResult(idx, 0), Argument::NestedResult(idx, 1)],
    );

    submit_execute_write(client, signer, pt, p.gas_budget).await
}

/// Build + sign + submit the trader-flow PTB.
///
/// Symmetric to [`execute_writer_flow`], but the executor is the *retail
/// trader*: they supply the premium (minted from the settlement faucet inside
/// the PTB); the Writer MM provides the underlying from their Account. The
/// contract transfers the Position to the quote's `signer_token_recipient`
/// (the MM) and returns the `Coin<Call>` to the PTB, which routes it to
/// `executor_recipient`.
pub async fn execute_trader_flow(
    client: &SuiClient,
    signer: &Signer,
    p: &ExecuteTraderParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(
        %p.package,
        %p.bucket_id,
        write_amount = p.write_amount,
        premium = p.premium,
        nonce = p.nonce,
        "building execute_write (trader flow) PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // Shared object args.
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let org = pt.obj(shared_object_arg(client, p.org_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let mm_account = pt.obj(shared_object_arg(client, p.mm_account_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?;

    // Pure inputs.
    let arg_protocol_id = pt.pure(&p.protocol_id)?;
    let arg_signer_acct_id = pt.pure(&p.signer_account_id_bytes)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id_bytes)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(&p.signature)?;
    let arg_mint_amount = pt.pure(&p.premium)?;

    // Type tags.
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let c_tag = TypeTag::from_str(p.call_type)
        .with_context(|| format!("parsing call type {}", p.call_type))?;

    // 1. test_tokens::<module>::mint(faucet, premium) -> Coin<Settlement>
    let coin_settlement = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_mint_amount],
    );

    // 2. quote::new_quote(...)
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

    // 3. quote::new_signed_quote(quote, signature)
    let signed_quote = pt.programmable_move_call(
        p.package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_signed_quote").unwrap(),
        vec![],
        vec![quote_val, arg_signature],
    );

    // 4. bucket::execute_write_trader_flow<U, S, Call>(...)
    //    -> Coin<Call> returned to the PTB.
    let call_coin = pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("execute_write_trader_flow").unwrap(),
        vec![u_tag, s_tag, c_tag],
        vec![
            bucket,
            org,
            config,
            treasury,
            mm_account,
            coin_settlement,
            signed_quote,
            clock,
        ],
    );

    // 5. Route the returned Coin<Call> to the executor.
    pt.transfer_args(p.executor_recipient, vec![call_coin]);

    submit_execute_write(client, signer, pt, p.gas_budget).await
}

/// Gas-select, sign, submit, and assert success for an `execute_write` PTB.
/// Shared by both the writer and trader flows.
async fn submit_execute_write(
    client: &SuiClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<SuiTransactionBlockResponse> {
    let programmable = pt.finish();

    // Gas selection.
    let gas_coin = client
        .coin_read_api()
        .get_coins(signer.address, None, None, Some(5))
        .await
        .context("listing gas coins")?
        .data
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
        gas_budget,
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
    debug!(digest = %resp.digest, "execute_write succeeded");
    Ok(resp)
}
