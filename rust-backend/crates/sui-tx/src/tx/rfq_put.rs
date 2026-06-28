//! PTB builders for the on-chain put RFQ (`rfq_put.move`) — the mirror of
//! [`super::rfq`]. The collateral leg is cash (settlement); the bid leg is the
//! premium (also settlement). Only the module name (`rfq_put`) and `create`'s
//! extra `amount` arg differ from the call builders.

use std::str::FromStr;

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_sdk::SuiClient;
use sui_types::base_types::{ObjectID, SuiAddress};
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Command;
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::rfq::clock_arg;
use crate::tx::{owned_object_arg, shared_object_arg, submit_ptb};

/// The (Underlying, Settlement, Put) type triple every put-rfq call is
/// generic over.
pub struct PutRfqTypes<'a> {
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub put_type: &'a str,
}

impl PutRfqTypes<'_> {
    fn tags(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![
            TypeTag::from_str(self.underlying_type)
                .with_context(|| format!("parsing underlying type {}", self.underlying_type))?,
            TypeTag::from_str(self.settlement_type)
                .with_context(|| format!("parsing settlement type {}", self.settlement_type))?,
            TypeTag::from_str(self.put_type)
                .with_context(|| format!("parsing put type {}", self.put_type))?,
        ])
    }
}

pub struct PutRfqBidParams<'a> {
    pub package: ObjectID,
    pub types: PutRfqTypes<'a>,
    pub rfq_id: ObjectID,
    /// Owned `Coin<Settlement>` the bid is split out of.
    pub funding_coin: ObjectID,
    pub premium: u64,
    /// Where the `Coin<Put>` goes if this bid wins.
    pub put_recipient: SuiAddress,
    pub gas_budget: u64,
}

/// `rfq_put::bid`: split the premium out of `funding_coin` and escrow it.
pub async fn bid(
    client: &SuiClient,
    signer: &Signer,
    p: &PutRfqBidParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, premium = p.premium, "building rfq_put::bid PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;
    let amount = pt.pure(&p.premium)?;
    let put_recipient = pt.pure(&p.put_recipient)?;

    let bid_coin = pt.command(Command::SplitCoins(funding, vec![amount]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq_put").unwrap(),
        Identifier::new("bid").unwrap(),
        p.types.tags()?,
        vec![rfq, bid_coin, put_recipient, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq_put::bid").await
}

pub struct PutRfqSettleParams<'a> {
    pub package: ObjectID,
    pub types: PutRfqTypes<'a>,
    pub rfq_id: ObjectID,
    pub bucket_id: ObjectID,
    pub protocol_config_id: ObjectID,
    pub treasury_id: ObjectID,
    pub gas_budget: u64,
}

/// `rfq_put::settle`: resolve a closed standalone put auction.
pub async fn settle(
    client: &SuiClient,
    signer: &Signer,
    p: &PutRfqSettleParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, "building rfq_put::settle PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, true).await?)?;
    let config = pt.obj(shared_object_arg(client, p.protocol_config_id, false).await?)?;
    let treasury = pt.obj(shared_object_arg(client, p.treasury_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq_put").unwrap(),
        Identifier::new("settle").unwrap(),
        p.types.tags()?,
        vec![rfq, bucket, config, treasury, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq_put::settle").await
}

pub struct PutRfqSettleExpiredParams<'a> {
    pub package: ObjectID,
    pub types: PutRfqTypes<'a>,
    pub rfq_id: ObjectID,
    pub bucket_id: ObjectID,
    pub gas_budget: u64,
}

/// `rfq_put::settle_expired`: refund both escrows of a standalone put auction.
pub async fn settle_expired(
    client: &SuiClient,
    signer: &Signer,
    p: &PutRfqSettleExpiredParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(rfq = %p.rfq_id, "building rfq_put::settle_expired PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let rfq = pt.obj(shared_object_arg(client, p.rfq_id, true).await?)?;
    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, false).await?)?;
    let clock = clock_arg(&mut pt)?;

    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq_put").unwrap(),
        Identifier::new("settle_expired").unwrap(),
        p.types.tags()?,
        vec![rfq, bucket, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "rfq_put::settle_expired").await
}

pub struct PutRfqCreateParams<'a> {
    pub package: ObjectID,
    pub types: PutRfqTypes<'a>,
    pub bucket_id: ObjectID,
    /// Owned `Coin<Settlement>` the collateral is split out of.
    pub funding_coin: ObjectID,
    /// Put notional in underlying units.
    pub amount: u64,
    /// Cash collateral to escrow = ceil(amount × strike).
    pub collateral: u64,
    pub reserve_premium: u64,
    pub duration_ms: u64,
    pub snipe_window_ms: u64,
    pub snipe_extension_ms: u64,
    pub max_extension_ms: u64,
    pub min_increment_bps: u64,
    pub position_recipient: SuiAddress,
    pub proceeds_recipient: SuiAddress,
    pub origin: ObjectID,
    pub gas_budget: u64,
}

/// `rfq_put::create`: open a standalone cash-secured-put auction. Unlike the
/// call `create`, the escrow is settlement collateral and the underlying
/// notional `amount` is an explicit argument.
pub async fn create(
    client: &SuiClient,
    signer: &Signer,
    p: &PutRfqCreateParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(bucket = %p.bucket_id, amount = p.amount, collateral = p.collateral, "building rfq_put::create PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let bucket = pt.obj(shared_object_arg(client, p.bucket_id, false).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;

    let collateral_amt = pt.pure(&p.collateral)?;
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

    let collateral = pt.command(Command::SplitCoins(funding, vec![collateral_amt]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("rfq_put").unwrap(),
        Identifier::new("create").unwrap(),
        p.types.tags()?,
        vec![
            bucket,
            collateral,
            amount,
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

    submit_ptb(client, signer, pt, p.gas_budget, "rfq_put::create").await
}
