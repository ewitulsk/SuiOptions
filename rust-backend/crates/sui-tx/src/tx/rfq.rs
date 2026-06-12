//! PTB builders for the on-chain RFQ (`rfq.move`) — consumed by the
//! mm-bot's on-chain bidder and the vault keeper (vault-implementation-
//! guide doc 05 §5). Standalone-auction calls only; the vault-coupled
//! cranks (`open_rfq`, `settle_rfq`, …) live in [`super::vault`].

use std::str::FromStr;

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::{Argument, Command, ObjectArg, SharedObjectMutability};
use sui_types::{SUI_CLOCK_OBJECT_ID, SUI_CLOCK_OBJECT_SHARED_VERSION};
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::{owned_object_arg, shared_object_arg, submit_ptb};

/// The (Underlying, Settlement, Call) type triple every rfq call is
/// generic over.
pub struct RfqTypes<'a> {
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub call_type: &'a str,
}

impl RfqTypes<'_> {
    pub(crate) fn tags(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![
            TypeTag::from_str(self.underlying_type)
                .with_context(|| format!("parsing underlying type {}", self.underlying_type))?,
            TypeTag::from_str(self.settlement_type)
                .with_context(|| format!("parsing settlement type {}", self.settlement_type))?,
            TypeTag::from_str(self.call_type)
                .with_context(|| format!("parsing call type {}", self.call_type))?,
        ])
    }
}

pub(crate) fn clock_arg(
    pt: &mut ProgrammableTransactionBuilder,
) -> Result<Argument> {
    Ok(pt.obj(ObjectArg::SharedObject {
        id: SUI_CLOCK_OBJECT_ID,
        initial_shared_version: SUI_CLOCK_OBJECT_SHARED_VERSION,
        mutability: SharedObjectMutability::Immutable,
    })?)
}

pub struct RfqBidParams<'a> {
    pub package: ObjectID,
    pub types: RfqTypes<'a>,
    pub rfq_id: ObjectID,
    /// Owned `Coin<Settlement>` the bid is split out of (the remainder
    /// stays with the signer).
    pub funding_coin: ObjectID,
    /// Escrowed bid amount, settlement smallest-units.
    pub premium: u64,
    /// Where the `Coin<Call>` goes if this bid wins.
    pub call_recipient: SuiAddress,
    pub gas_budget: u64,
}

/// `rfq::bid`: split the premium out of `funding_coin` and escrow it.
pub async fn bid(
    client: &SuiClient,
    signer: &Signer,
    p: &RfqBidParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, premium = p.premium, "building rfq::bid PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;
    let amount = pt.pure(&p.premium)?;
    let call_recipient = pt.pure(&p.call_recipient)?;

    let bid_coin = pt.command(Command::SplitCoins(funding, vec![amount]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq").unwrap(),
        Identifier::new("bid").unwrap(),
        p.types.tags()?,
        vec![rfq, bid_coin, call_recipient, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq::bid").await
}

pub struct RfqSettleParams<'a> {
    pub package: ObjectID,
    pub types: RfqTypes<'a>,
    pub rfq_id: ObjectID,
    pub bucket_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub treasury_id: ObjectID,
    pub gas_budget: u64,
}

/// `rfq::settle`: resolve a closed standalone auction (consumes the
/// shared auction object). Callable by anyone.
pub async fn settle(
    client: &SuiClient,
    signer: &Signer,
    p: &RfqSettleParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, "building rfq::settle PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq").unwrap(),
        Identifier::new("settle").unwrap(),
        p.types.tags()?,
        vec![rfq, bucket, config, treasury, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq::settle").await
}

pub struct RfqSettleExpiredParams<'a> {
    pub package: ObjectID,
    pub types: RfqTypes<'a>,
    pub rfq_id: ObjectID,
    pub bucket_id: ObjectID,
    pub gas_budget: u64,
}

/// `rfq::settle_expired`: refund both escrows of a standalone auction
/// whose bucket expired or was invalidated mid-auction.
pub async fn settle_expired(
    client: &SuiClient,
    signer: &Signer,
    p: &RfqSettleExpiredParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, "building rfq::settle_expired PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, false).await?)?;
    let clock = clock_arg(&mut pt)?;

    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq").unwrap(),
        Identifier::new("settle_expired").unwrap(),
        p.types.tags()?,
        vec![rfq, bucket, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq::settle_expired").await
}

pub struct RfqCreateParams<'a> {
    pub package: ObjectID,
    pub types: RfqTypes<'a>,
    pub bucket_id: ObjectID,
    /// Owned `Coin<Underlying>` the slice is split out of.
    pub funding_coin: ObjectID,
    pub amount: u64,
    pub reserve_premium: u64,
    pub duration_ms: u64,
    pub snipe_window_ms: u64,
    pub snipe_extension_ms: u64,
    pub max_extension_ms: u64,
    pub min_increment_bps: u64,
    pub position_recipient: SuiAddress,
    pub proceeds_recipient: SuiAddress,
    /// Attribution id (seller address-as-ID for standalone use).
    pub origin: ObjectID,
    pub gas_budget: u64,
}

/// `rfq::create`: open a standalone covered-write auction (the vault path
/// goes through `vault::open_rfq` instead).
pub async fn create(
    client: &SuiClient,
    signer: &Signer,
    p: &RfqCreateParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(bucket = %p.bucket_id, amount = p.amount, "building rfq::create PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, false).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;

    let amount = pt.pure(&p.amount)?;
    let reserve = pt.pure(&p.reserve_premium)?;
    let duration = pt.pure(&p.duration_ms)?;
    let snipe_window = pt.pure(&p.snipe_window_ms)?;
    let snipe_extension = pt.pure(&p.snipe_extension_ms)?;
    let max_extension = pt.pure(&p.max_extension_ms)?;
    let min_increment = pt.pure(&p.min_increment_bps)?;
    let position_recipient = pt.pure(&p.position_recipient)?;
    let proceeds_recipient = pt.pure(&p.proceeds_recipient)?;
    let origin = pt.pure(&p.origin.into_bytes())?;

    let slice = pt.command(Command::SplitCoins(funding, vec![amount]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq").unwrap(),
        Identifier::new("create").unwrap(),
        p.types.tags()?,
        vec![
            bucket,
            slice,
            reserve,
            duration,
            snipe_window,
            snipe_extension,
            max_extension,
            min_increment,
            position_recipient,
            proceeds_recipient,
            origin,
            clock,
        ],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq::create").await
}
