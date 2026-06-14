//! PTB builder for the proceeds-swap auction (`swap_auction.move`) —
//! consumed by the mm-bot's on-chain bidder (vault-implementation-guide
//! doc 05 §3). The vault-coupled cranks (`open_swap_rfq`,
//! `settle_swap_rfq`) live in [`super::vault`]; only `bid` is public to
//! outside callers.

use std::str::FromStr;

use anyhow::{Context, Result};
use move_core_types::identifier::Identifier;
use move_core_types::language_storage::TypeTag;
use sui_json_rpc_types::SuiTransactionBlockResponse;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;
use sui_types::programmable_transaction_builder::ProgrammableTransactionBuilder;
use sui_types::transaction::Command;
use tracing::info;

use crate::sui_client::Signer;
use crate::tx::rfq::clock_arg;
use crate::tx::{owned_object_arg, shared_object_arg, submit_ptb};

pub struct SwapBidParams<'a> {
    pub package: ObjectID,
    pub underlying_type: &'a str,
    pub settlement_type: &'a str,
    pub swap_id: ObjectID,
    /// Owned `Coin<Underlying>` the bid is split out of (the remainder
    /// stays with the signer).
    pub funding_coin: ObjectID,
    /// Escrowed bid amount, underlying smallest-units.
    pub underlying: u64,
    pub gas_budget: u64,
}

impl SwapBidParams<'_> {
    fn tags(&self) -> Result<Vec<TypeTag>> {
        Ok(vec![
            TypeTag::from_str(self.underlying_type)
                .with_context(|| format!("parsing underlying type {}", self.underlying_type))?,
            TypeTag::from_str(self.settlement_type)
                .with_context(|| format!("parsing settlement type {}", self.settlement_type))?,
        ])
    }
}

/// `swap_auction::bid`: split `underlying` out of `funding_coin` and
/// escrow it as a bid (the winner receives the vault's settlement). The
/// bidder receives any refund/payout at their own address.
pub async fn bid(
    client: &SuiClient,
    signer: &Signer,
    p: &SwapBidParams<'_>,
) -> Result<SuiTransactionBlockResponse> {
    info!(swap = %p.swap_id, underlying = p.underlying, "building swap_auction::bid PTB");
    let mut pt = ProgrammableTransactionBuilder::new();

    let auction = pt.obj(shared_object_arg(client, p.swap_id, true).await?)?;
    let funding = pt.obj(owned_object_arg(client, p.funding_coin).await?)?;
    let clock = clock_arg(&mut pt)?;
    let amount = pt.pure(&p.underlying)?;

    let bid_coin = pt.command(Command::SplitCoins(funding, vec![amount]));
    pt.programmable_move_call(
        p.package,
        Identifier::new("swap_auction").unwrap(),
        Identifier::new("bid").unwrap(),
        p.tags()?,
        vec![auction, bid_coin, clock],
    );

    submit_ptb(client, signer, pt, p.gas_budget, "swap_auction::bid").await
}
