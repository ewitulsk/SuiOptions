//! Generic escrowed ascending-auction venue — audit package 2.
//!
//! The unification of `rfq.move`, `rfq_put.move` and `swap_auction.move`
//! (docs/solana/solana-port-plan.md §5.2): one auction machine (escrowed
//! bids, reserve floor, strict increment, anti-snipe, permissionless
//! settle) with three settle modes. Pure-swap mode has ZERO dependency on
//! options_core — the venue is a standalone token-auction product; the
//! covered-call / cash-secured-put adapters CPI core's audited
//! `write_collateralized` surface. `settle_authority` ports the Move
//! `coupled` flag: a venue built on top (the vault) gates settlement
//! behind its own PDA signature and absorbs outputs into token accounts
//! fixed at creation.

pub mod error;
pub mod events;
pub mod instructions;
pub mod state;
pub mod util;

use anchor_lang::prelude::*;

pub use instructions::*;
pub use state::*;

declare_id!("8cvpWnJaQ4kTEPypwrZvBPzEM4R7FbivgybXBm2ahvKk");

#[program]
pub mod auction_venue {
    use super::*;

    pub fn create_swap_auction(
        ctx: Context<CreateAuction>,
        salt: u64,
        escrow_amount: u64,
        params: AuctionParams,
    ) -> Result<()> {
        instructions::create::handle_create_swap_auction(ctx, salt, escrow_amount, params)
    }

    pub fn create_call_auction(
        ctx: Context<CreateAuction>,
        salt: u64,
        escrow_amount: u64,
        params: AuctionParams,
    ) -> Result<()> {
        instructions::create::handle_create_call_auction(ctx, salt, escrow_amount, params)
    }

    pub fn create_put_auction(
        ctx: Context<CreateAuction>,
        salt: u64,
        notional: u64,
        params: AuctionParams,
    ) -> Result<()> {
        instructions::create::handle_create_put_auction(ctx, salt, notional, params)
    }

    pub fn bid(ctx: Context<Bid>, amount: u64, token_recipient: Pubkey) -> Result<()> {
        instructions::bid::handle_bid(ctx, amount, token_recipient)
    }

    pub fn settle_call(ctx: Context<SettleCall>) -> Result<()> {
        instructions::settle::handle_settle_call(ctx)
    }

    pub fn settle_put(ctx: Context<SettlePut>) -> Result<()> {
        instructions::settle::handle_settle_put(ctx)
    }

    pub fn settle_swap(ctx: Context<SettleSwap>, force_refund: bool) -> Result<()> {
        instructions::settle::handle_settle_swap(ctx, force_refund)
    }

    pub fn settle_expired(ctx: Context<SettleExpired>) -> Result<()> {
        instructions::settle::handle_settle_expired(ctx)
    }
}
