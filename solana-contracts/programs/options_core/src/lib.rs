//! Options protocol core — audit package 1.
//!
//! Port of the Sui Move `options_protocol` package's core modules
//! (`bucket`, `put_bucket`, `position`, `account`, `quote`, `admin`,
//! `treasury`) per docs/solana/solana-port-plan.md. Fully functional
//! standalone: the quote-based `execute_write` MM flow and the
//! `write_collateralized` self-write primitive need no auction venue.
//! The venue (audit package 2) and vault (audit package 3) build on this
//! program via CPI and are never referenced here.

pub mod error;
pub mod events;
pub mod instructions;
pub mod quote;
pub mod state;
pub mod util;

use anchor_lang::prelude::*;

pub use instructions::*;
pub use state::*;

declare_id!("6KeiQVrkr7uxW1LKhZGpjg7yaYVrz4AKyGaD7Dgnef1t");

#[program]
pub mod options_core {
    use super::*;

    // ── admin / config / treasury ──

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        instructions::admin::handle_initialize(ctx)
    }

    pub fn set_fee_bps(ctx: Context<AdminConfig>, new_bps: u64) -> Result<()> {
        instructions::admin::handle_set_fee_bps(ctx, new_bps)
    }

    pub fn set_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        instructions::admin::handle_set_admin(ctx, new_admin)
    }

    pub fn withdraw_treasury(ctx: Context<WithdrawTreasury>, amount: u64) -> Result<()> {
        instructions::admin::handle_withdraw_treasury(ctx, amount)
    }

    pub fn deposit_protocol_fee(ctx: Context<DepositProtocolFee>, amount: u64) -> Result<()> {
        instructions::admin::handle_deposit_protocol_fee(ctx, amount)
    }

    // ── MM accounts ──

    pub fn create_account(
        ctx: Context<CreateAccount>,
        salt: u64,
        signing_scheme: u8,
        signing_pubkey: Vec<u8>,
    ) -> Result<()> {
        instructions::account::handle_create_account(ctx, salt, signing_scheme, signing_pubkey)
    }

    pub fn account_deposit(ctx: Context<DepositToAccount>, amount: u64) -> Result<()> {
        instructions::account::handle_account_deposit(ctx, amount)
    }

    pub fn account_withdraw(ctx: Context<WithdrawFromAccount>, amount: u64) -> Result<()> {
        instructions::account::handle_account_withdraw(ctx, amount)
    }

    pub fn rotate_signing_key(
        ctx: Context<RotateSigningKey>,
        new_scheme: u8,
        new_pubkey: Vec<u8>,
    ) -> Result<()> {
        instructions::account::handle_rotate_signing_key(ctx, new_scheme, new_pubkey)
    }

    pub fn prune_nonce(ctx: Context<PruneNonce>) -> Result<()> {
        instructions::account::handle_prune_nonce(ctx)
    }

    // ── call buckets ──

    pub fn create_bucket(
        ctx: Context<CreateBucket>,
        salt: u64,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
    ) -> Result<()> {
        instructions::bucket_admin::handle_create_bucket(ctx, salt, expiry_ms, strike, strike_scale)
    }

    pub fn invalidate_bucket(ctx: Context<ToggleBucketValidity>, reason: String) -> Result<()> {
        instructions::bucket_admin::handle_invalidate_bucket(ctx, reason)
    }

    pub fn revalidate_bucket(ctx: Context<ToggleBucketValidity>, reason: String) -> Result<()> {
        instructions::bucket_admin::handle_revalidate_bucket(ctx, reason)
    }

    pub fn cleanup_bucket(ctx: Context<CleanupBucket>) -> Result<()> {
        instructions::bucket_admin::handle_cleanup_bucket(ctx)
    }

    pub fn write_collateralized(
        ctx: Context<WriteCollateralized>,
        amount: u64,
        position_owner: Pubkey,
    ) -> Result<()> {
        instructions::bucket_write::handle_write_collateralized(ctx, amount, position_owner)
    }

    pub fn exercise(ctx: Context<Exercise>, amount: u64) -> Result<()> {
        instructions::bucket_settle::handle_exercise(ctx, amount)
    }

    pub fn redeem_position(ctx: Context<RedeemPosition>) -> Result<()> {
        instructions::bucket_settle::handle_redeem_position(ctx)
    }

    pub fn burn_expired_option(ctx: Context<BurnExpiredOption>, amount: u64) -> Result<()> {
        instructions::bucket_settle::handle_burn_expired_option(ctx, amount)
    }

    pub fn transfer_position(ctx: Context<TransferPosition>, new_owner: Pubkey) -> Result<()> {
        instructions::bucket_settle::handle_transfer_position(ctx, new_owner)
    }

    pub fn execute_write(
        ctx: Context<ExecuteWrite>,
        quote: crate::quote::Quote,
        flow: crate::quote::FlowKind,
        position_recipient: Pubkey,
        sig_ix_index: u8,
    ) -> Result<()> {
        instructions::execute_write::handle_execute_write(
            ctx,
            quote,
            flow,
            position_recipient,
            sig_ix_index,
        )
    }

    // ── put buckets ──

    pub fn create_put_bucket(
        ctx: Context<CreatePutBucket>,
        salt: u64,
        expiry_ms: u64,
        strike: u128,
        strike_scale: u8,
    ) -> Result<()> {
        instructions::put_admin::handle_create_put_bucket(ctx, salt, expiry_ms, strike, strike_scale)
    }

    pub fn invalidate_put_bucket(
        ctx: Context<TogglePutBucketValidity>,
        reason: String,
    ) -> Result<()> {
        instructions::put_admin::handle_invalidate_put_bucket(ctx, reason)
    }

    pub fn revalidate_put_bucket(
        ctx: Context<TogglePutBucketValidity>,
        reason: String,
    ) -> Result<()> {
        instructions::put_admin::handle_revalidate_put_bucket(ctx, reason)
    }

    pub fn cleanup_put_bucket(ctx: Context<CleanupPutBucket>) -> Result<()> {
        instructions::put_admin::handle_cleanup_put_bucket(ctx)
    }

    pub fn write_put_collateralized(
        ctx: Context<WritePutCollateralized>,
        write_amount: u64,
        position_owner: Pubkey,
    ) -> Result<()> {
        instructions::put_write::handle_write_put_collateralized(ctx, write_amount, position_owner)
    }

    pub fn execute_put_write(
        ctx: Context<ExecutePutWrite>,
        quote: crate::quote::Quote,
        flow: crate::quote::FlowKind,
        position_recipient: Pubkey,
        sig_ix_index: u8,
    ) -> Result<()> {
        instructions::put_write::handle_execute_put_write(
            ctx,
            quote,
            flow,
            position_recipient,
            sig_ix_index,
        )
    }

    pub fn exercise_put(ctx: Context<ExercisePut>, amount: u64) -> Result<()> {
        instructions::put_settle::handle_exercise_put(ctx, amount)
    }

    pub fn redeem_put_position(ctx: Context<RedeemPutPosition>) -> Result<()> {
        instructions::put_settle::handle_redeem_put_position(ctx)
    }

    pub fn burn_expired_put(ctx: Context<BurnExpiredPut>, amount: u64) -> Result<()> {
        instructions::put_settle::handle_burn_expired_put(ctx, amount)
    }
}
