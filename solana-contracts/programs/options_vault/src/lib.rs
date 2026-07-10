//! Covered-call vault — audit package 3.
//!
//! Port of `vault.move` (docs/solana/solana-port-plan.md §5.3):
//! Ribbon-style weekly rounds over the on-chain auction venue, with the
//! same three principles — permissionless lifecycle cranks (the keeper
//! has zero privileges), the state machine as the spec, and
//! oracle-bounded discretion (Pyth bands every degree of freedom a crank
//! has). Accounting mirrors `vault-sim::ledger` unit-for-unit.
//!
//! Every in-package interaction from Move becomes a CPI: covered writes
//! and proceeds swaps run through coupled `auction_venue` auctions gated
//! on this vault's PDA; redemption CPIs `options_core` directly. The
//! Pyth oracle module lives HERE — core and venue never price anything.

pub mod error;
pub mod events;
pub mod instructions;
pub mod oracle;
pub mod state;
pub mod util;

use anchor_lang::prelude::*;

pub use instructions::*;
pub use state::*;

declare_id!("ELxbfwPUPJ4U1SnvWZJpLxdCRbgMiBpgQmdRizNWYcXe");

#[program]
pub mod options_vault {
    use super::*;

    // ── admin ──

    pub fn create_vault(ctx: Context<CreateVault>, salt: u64, config: VaultConfig) -> Result<()> {
        instructions::admin::handle_create_vault(ctx, salt, config)
    }

    pub fn update_config(ctx: Context<VaultAdmin>, new_config: VaultConfig) -> Result<()> {
        instructions::admin::handle_update_config(ctx, new_config)
    }

    pub fn update_oracle_feeds(
        ctx: Context<VaultAdmin>,
        underlying_feed_id: [u8; 32],
        settlement_feed_id: [u8; 32],
    ) -> Result<()> {
        instructions::admin::handle_update_oracle_feeds(ctx, underlying_feed_id, settlement_feed_id)
    }

    pub fn set_paused(ctx: Context<VaultAdmin>, paused: bool) -> Result<()> {
        instructions::admin::handle_set_paused(ctx, paused)
    }

    // ── users ──

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        instructions::user::handle_deposit(ctx, amount)
    }

    pub fn claim_shares(ctx: Context<ClaimShares>) -> Result<()> {
        instructions::user::handle_claim_shares(ctx)
    }

    pub fn initiate_withdraw(ctx: Context<InitiateWithdraw>, shares: u64) -> Result<()> {
        instructions::user::handle_initiate_withdraw(ctx, shares)
    }

    pub fn complete_withdraw(ctx: Context<CompleteWithdraw>) -> Result<()> {
        instructions::user::handle_complete_withdraw(ctx)
    }

    pub fn instant_withdraw_pending(ctx: Context<InstantWithdrawPending>) -> Result<()> {
        instructions::user::handle_instant_withdraw_pending(ctx)
    }

    // ── lifecycle cranks (permissionless) ──

    pub fn select_bucket(ctx: Context<SelectBucket>) -> Result<()> {
        instructions::select::handle_select_bucket(ctx)
    }

    pub fn crank_redeem(ctx: Context<CrankRedeem>) -> Result<()> {
        instructions::select::handle_crank_redeem(ctx)
    }

    pub fn open_rfq(ctx: Context<OpenRfq>, slice_amount: u64) -> Result<()> {
        instructions::rfq::handle_open_rfq(ctx, slice_amount)
    }

    pub fn settle_rfq(ctx: Context<SettleRfq>) -> Result<()> {
        instructions::rfq::handle_settle_rfq(ctx)
    }

    pub fn settle_rfq_expired(ctx: Context<SettleRfqExpired>) -> Result<()> {
        instructions::rfq::handle_settle_rfq_expired(ctx)
    }

    pub fn open_swap_rfq(ctx: Context<OpenSwapRfq>, amount_s: u64) -> Result<()> {
        instructions::swap::handle_open_swap_rfq(ctx, amount_s)
    }

    pub fn settle_swap_rfq(ctx: Context<SettleSwapRfq>) -> Result<()> {
        instructions::swap::handle_settle_swap_rfq(ctx)
    }

    pub fn finalize_round(ctx: Context<FinalizeRound>) -> Result<()> {
        instructions::finalize::handle_finalize_round(ctx)
    }
}
