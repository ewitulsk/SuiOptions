//! Programmable transactions for the covered-call collateral protocol
//! (docs/audit-restructure/04-collateral-abstraction-plan.md §5).
//!
//! Single PTB per flow, four protocol steps plus the executor's faucet mint:
//!
//! ```text
//! 1. test_tokens::<module>::mint(faucet, amount)            -> Coin<T> (executor side)
//! 2. quote::new_quote(...)                                  -> Quote
//! 3. quote::new_signed_quote(q, sig)                        -> SignedQuote
//! 4. bucket::request_writer_flow<U,S,C>(bucket, signer,
//!    config, sq, clock)                                     -> CollateralRequest
//! 5. {release_package}::{release_module}::release<T>(
//!    collateral_account, &request, ctx)                     -> Balance<T>
//! 6. bucket::execute_writer_flow<U,S,C>(bucket, config, wl,
//!    treasury, request, funds, coin, recipient, clock)
//! ```
//!
//! The release call targets a RUNTIME package/module taken straight from the
//! MM's SIGNED quote — no unsigned routing envelope exists. The faucet mint
//! composes with the rest of the PTB because the test-token
//! `mint(&mut Faucet, u64, ctx): Coin<T>` is a non-entry public function —
//! its result is addressable as a PTB `Argument`.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{
    Argument, ObjectArg, SharedObjectMutability,
};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::{debug, info};

use crate::sui_client::Signer;
use crate::tx::shared_object_arg;
use crate::chain::{ChainClient, ExecutedTransaction};

/// The quote fields the MM signed over plus the routing objects the PTB
/// resolves from them. Shared by both flows (and both products).
pub struct QuoteRouting<'a> {
    /// Shared `QuoteSigner` object (== the quote's `signer_id`).
    pub quote_signer_id: ObjectID,
    /// Shared collateral object `release()` debits
    /// (== the quote's `collateral_source`).
    pub collateral_account_id: ObjectID,
    /// Package containing the MM's `release` implementation
    /// (== the quote's `release_package`).
    pub release_package: ObjectID,
    /// Module containing `release` (== the quote's `release_module`).
    pub release_module: &'a str,
}

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
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Writer flow: the executor (retail writer) receives the Position. The
    /// call coin goes to the quote's `signer_token_recipient` on chain.
    pub position_recipient: SuiAddress,

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
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,

    /// Trader flow: the retail trader receives the CallOption coin. The
    /// Position goes to the quote's `signer_token_recipient` on chain.
    pub call_token_recipient: SuiAddress,

    pub gas_budget: u64,
}

/// The shared quote → request → release prelude (steps 2–5). Returns
/// `(request, funds)` arguments for the `execute_*_flow` call.
///
/// `request_module` is `"bucket"` for calls / `"put_bucket"` for puts;
/// `release_type` is the coin type the potato demands (`Settlement` for a
/// writer flow, `Underlying` for a call trader flow, `Settlement` for both
/// put flows).
pub(crate) struct FlowPrelude<'a> {
    pub package: ObjectID,
    pub request_module: &'a str,
    pub request_function: &'a str,
    pub request_type_args: Vec<TypeTag>,
    pub release_type: TypeTag,
    pub routing: &'a QuoteRouting<'a>,
    pub protocol_id: &'a [u8],
    pub signer_token_recipient: SuiAddress,
    pub bucket_id: ObjectID,
    pub write_amount: u64,
    pub premium: u64,
    pub valid_until_ms: u64,
    pub nonce: u64,
    pub signature: &'a [u8],
}

/// Build steps 2–5 into `pt`. `bucket`/`config`/`clock` are the already-
/// registered shared-object arguments (the bucket input is shared with the
/// later execute call).
pub(crate) async fn build_request_and_release(
    client: &ChainClient,
    pt: &mut ProgrammableTransactionBuilder,
    p: FlowPrelude<'_>,
    bucket: Argument,
    config: Argument,
    clock: Argument,
) -> Result<(Argument, Argument)> {
    // Shared objects owned by this prelude: the QuoteSigner (nonce burn) and
    // the MM's collateral account (both mutable).
    let quote_signer =
        pt.obj(shared_object_arg(client, p.routing.quote_signer_id, true).await?)?;
    let collateral_account =
        pt.obj(shared_object_arg(client, p.routing.collateral_account_id, true).await?)?;

    // Pure quote fields — BCS byte-for-byte what the MM signed. `ObjectID`
    // and `SuiAddress` BCS-encode as bare 32 bytes; a Rust `&str` encodes as
    // ULEB length + utf8 bytes, matching Move's `std::string::String`.
    let arg_protocol_id = pt.pure(p.protocol_id)?;
    let arg_signer_id = pt.pure(&p.routing.quote_signer_id)?;
    let arg_collateral_source = pt.pure(&p.routing.collateral_account_id)?;
    let arg_release_package = pt.pure(&p.routing.release_package)?;
    let arg_release_module = pt.pure(p.routing.release_module)?;
    let arg_signer_token_recipient = pt.pure(&p.signer_token_recipient)?;
    let arg_bucket_id = pt.pure(&p.bucket_id)?;
    let arg_write_amount = pt.pure(&p.write_amount)?;
    let arg_premium = pt.pure(&p.premium)?;
    let arg_valid_until_ms = pt.pure(&p.valid_until_ms)?;
    let arg_nonce = pt.pure(&p.nonce)?;
    let arg_signature = pt.pure(p.signature)?;

    // quote::new_quote(...)
    let quote_val = pt.programmable_move_call(
        p.package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_quote").unwrap(),
        vec![],
        vec![
            arg_protocol_id,
            arg_signer_id,
            arg_collateral_source,
            arg_release_package,
            arg_release_module,
            arg_signer_token_recipient,
            arg_bucket_id,
            arg_write_amount,
            arg_premium,
            arg_valid_until_ms,
            arg_nonce,
        ],
    );

    // quote::new_signed_quote(quote, signature)
    let signed_quote = pt.programmable_move_call(
        p.package,
        Identifier::new("quote").unwrap(),
        Identifier::new("new_signed_quote").unwrap(),
        vec![],
        vec![quote_val, arg_signature],
    );

    // {bucket|put_bucket}::request_{writer|trader}_flow(...) -> potato
    let request = pt.programmable_move_call(
        p.package,
        Identifier::new(p.request_module)
            .map_err(|e| anyhow!("request module {}: {e}", p.request_module))?,
        Identifier::new(p.request_function).unwrap(),
        p.request_type_args,
        vec![bucket, quote_signer, config, signed_quote, clock],
    );

    // {release_package}::{release_module}::release<T>(account, &request, ctx)
    // — the MM-specified implementation, routed straight from the signed
    // quote. Returns Balance<T>.
    let funds = pt.programmable_move_call(
        p.routing.release_package,
        Identifier::new(p.routing.release_module)
            .map_err(|e| anyhow!("release module {}: {e}", p.routing.release_module))?,
        Identifier::new("release").unwrap(),
        vec![p.release_type],
        vec![collateral_account, request],
    );

    Ok((request, funds))
}

fn clock_arg(pt: &mut ProgrammableTransactionBuilder) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

/// Build + sign + submit the writer-flow PTB.
pub async fn execute_writer_flow(
    client: &ChainClient,
    signer: &Signer,
    p: &ExecuteWriteParams<'_>,
) -> Result<ExecutedTransaction> {
    info!(
        %p.package,
        %p.bucket_id,
        write_amount = p.write_amount,
        premium = p.premium,
        nonce = p.nonce,
        release_package = %p.routing.release_package,
        "building execute_writer_flow PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // Shared object args.
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, p.whitelist_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.underlying_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_position_recipient = pt.pure(&p.position_recipient)?;
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

    // 2–5. quote → signed quote → request (premium demanded in Settlement)
    // → release<Settlement>.
    let (request, funds) = build_request_and_release(
        client,
        &mut pt,
        FlowPrelude {
            package: p.package,
            request_module: "bucket",
            request_function: "request_writer_flow",
            request_type_args: vec![u_tag.clone(), s_tag.clone(), c_tag.clone()],
            release_type: s_tag.clone(),
            routing: &p.routing,
            protocol_id: &p.protocol_id,
            signer_token_recipient: p.signer_token_recipient,
            bucket_id: p.bucket_id,
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

    // 6. bucket::execute_writer_flow<U, S, C>(...)
    pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("execute_writer_flow").unwrap(),
        vec![u_tag, s_tag, c_tag],
        vec![
            bucket,
            config,
            wl,
            treasury,
            request,
            funds,
            coin_underlying,
            arg_position_recipient,
            clock,
        ],
    );

    submit_execute_write(client, signer, pt, p.gas_budget).await
}

/// Build + sign + submit the trader-flow PTB.
///
/// Symmetric to [`execute_writer_flow`], but the executor is the *retail
/// trader*: they supply the premium (minted from the settlement faucet inside
/// the PTB) and the underlying side is released from the Writer MM's
/// collateral account. The signer (MM) receives the Position at the quote's
/// `signer_token_recipient`; the trader receives the `CallOption` coin via
/// `call_token_recipient`.
pub async fn execute_trader_flow(
    client: &ChainClient,
    signer: &Signer,
    p: &ExecuteTraderParams<'_>,
) -> Result<ExecutedTransaction> {
    info!(
        %p.package,
        %p.bucket_id,
        write_amount = p.write_amount,
        premium = p.premium,
        nonce = p.nonce,
        release_package = %p.routing.release_package,
        "building execute_trader_flow PTB"
    );
    let mut pt = ProgrammableTransactionBuilder::new();

    // Shared object args.
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let wl = pt.obj(shared_object_arg(client, p.whitelist_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let faucet = pt.obj(shared_object_arg(client, p.settlement_faucet_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    let arg_call_token_recipient = pt.pure(&p.call_token_recipient)?;
    let arg_mint_amount = pt.pure(&p.premium)?;

    // Type tags.
    let u_tag = TypeTag::from_str(p.underlying_type)
        .with_context(|| format!("parsing underlying type {}", p.underlying_type))?;
    let s_tag = TypeTag::from_str(p.settlement_type)
        .with_context(|| format!("parsing settlement type {}", p.settlement_type))?;
    let c_tag = TypeTag::from_str(p.call_type)
        .with_context(|| format!("parsing call type {}", p.call_type))?;

    // 1. test_tokens::<module>::mint(faucet, premium) -> Coin<Settlement>
    let coin_premium = pt.programmable_move_call(
        p.tokens_package,
        Identifier::new(p.settlement_module)
            .map_err(|e| anyhow!("settlement module {}: {e}", p.settlement_module))?,
        Identifier::new("mint").unwrap(),
        vec![],
        vec![faucet, arg_mint_amount],
    );

    // 2–5. quote → signed quote → request (underlying demanded)
    // → release<Underlying>.
    let (request, funds) = build_request_and_release(
        client,
        &mut pt,
        FlowPrelude {
            package: p.package,
            request_module: "bucket",
            request_function: "request_trader_flow",
            request_type_args: vec![u_tag.clone(), s_tag.clone(), c_tag.clone()],
            release_type: u_tag.clone(),
            routing: &p.routing,
            protocol_id: &p.protocol_id,
            signer_token_recipient: p.signer_token_recipient,
            bucket_id: p.bucket_id,
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

    // 6. bucket::execute_trader_flow<U, S, C>(...)
    pt.programmable_move_call(
        p.package,
        Identifier::new("bucket").unwrap(),
        Identifier::new("execute_trader_flow").unwrap(),
        vec![u_tag, s_tag, c_tag],
        vec![
            bucket,
            config,
            wl,
            treasury,
            request,
            funds,
            coin_premium,
            arg_call_token_recipient,
            clock,
        ],
    );

    submit_execute_write(client, signer, pt, p.gas_budget).await
}

/// Gas-select, sign, submit, and assert success for an execute-flow PTB.
/// Shared by both the writer and trader flows.
async fn submit_execute_write(
    client: &ChainClient,
    signer: &Signer,
    pt: ProgrammableTransactionBuilder,
    gas_budget: u64,
) -> Result<ExecutedTransaction> {
    let resp = super::submit_ptb(client, signer, pt, gas_budget, "execute_write").await?;
    debug!(digest = %super::tx_digest(&resp), "execute_write succeeded");
    Ok(resp)
}
