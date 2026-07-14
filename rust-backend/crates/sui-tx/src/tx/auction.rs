//! PTB builders for the generic auction venue (`auction::auction`) — the
//! unification of the old `rfq` / `rfq_put` / `swap_auction` builders.
//! One shared `Auction<Escrow, Bid>` object covers every market: a
//! covered-call RFQ is `Auction<U, S>`, a cash-secured-put RFQ is
//! `Auction<S, S>`, and a vault proceeds swap is `Auction<S, U>`.
//! Consumed by the mm-bot's on-chain bidders; the vault-coupled cranks
//! (`open_rfq`, `settle_rfq`, …) live in [`super::vault`].

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
use crate::tx::{clock_arg, owned_object_arg, shared_object_arg, submit_ptb};

/// The (Escrow, Bid) type pair every auction call is generic over.
pub struct AuctionTypes<'a> {
    pub escrow_type: &'a str,
    pub bid_type: &'a str,
}

impl AuctionTypes<'_> {
    pub(crate) fn tags(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![
            TypeTag::from_str(self.escrow_type)
                .with_context(|| format!("parsing escrow type {}", self.escrow_type))?,
            TypeTag::from_str(self.bid_type)
                .with_context(|| format!("parsing bid type {}", self.bid_type))?,
        ])
    }
}

pub struct AuctionBidParams<'a> {
    /// The `auction` package id (from token-info `snapshot.auction()`).
    pub package: ObjectID,
    pub types: AuctionTypes<'a>,
    pub auction_id: ObjectID,
    /// Owned `Coin<Bid>` the bid is split out of (the remainder stays
    /// with the signer).
    pub funding_coin: ObjectID,
    /// Escrowed bid amount, bid-asset smallest-units.
    pub amount: u64,
    /// Where the escrow (uncoupled) or the venue-defined winnings
    /// (coupled: e.g. minted option coins) go if this bid wins.
    pub token_recipient: SuiAddress,
    pub gas_budget: u64,
}

/// `auction::bid`: split `amount` out of `funding_coin` and escrow it.
pub async fn bid(
    client: &SuiClient,
    signer: &Signer,
    p: &AuctionBidParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(auction = %p.auction_id, amount = p.amount, "building auction::bid PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let auction = pt.obj(shared_object_arg(client, p.auction_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;
    let amount = pt.pure(&p.amount)?;
    let token_recipient = pt.pure(&p.token_recipient)?;

    let bid_coin = pt.command(Command::SplitCoins(funding, vec![amount]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("auction").unwrap(),
        Identifier::new("bid").unwrap(),
        p.types.tags()?,
        vec![auction, bid_coin, token_recipient, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "auction::bid").await
}

pub struct AuctionSettleParams<'a> {
    pub package: ObjectID,
    pub types: AuctionTypes<'a>,
    pub auction_id: ObjectID,
    pub gas_budget: u64,
}

/// `auction::settle`: resolve a closed UNCOUPLED auction (consumes the
/// shared object). Callable by anyone; coupled auctions settle through
/// their venue (`vault::settle_rfq`, `options_rfq::settle_call`, …).
pub async fn settle(
    client: &SuiClient,
    signer: &Signer,
    p: &AuctionSettleParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(auction = %p.auction_id, "building auction::settle PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let auction = pt.obj(shared_object_arg(client, p.auction_id, true).await?)?;
    let clock = clock_arg(&mut pt)?;

    pt.programmable_move_call(
        p.package,
        Identifier::new("auction").unwrap(),
        Identifier::new("settle").unwrap(),
        p.types.tags()?,
        vec![auction, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "auction::settle").await
}
