use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount};

use crate::error::VaultError;
use crate::events::*;
use crate::state::*;
use crate::vault_seeds;

/// Queue a deposit for the next round (mirrors `vault::deposit`). Never
/// exposed to the current round's P&L. The receipt is a fresh keypair
/// account, like Sui's owned object.
#[event_cpi]
#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [PENDING_SEED, vault.key().as_ref()], bump)]
    pub pending: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = vault.underlying_mint)]
    pub depositor_token: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = depositor,
        space = 8 + DepositReceipt::INIT_SPACE,
        signer
    )]
    pub receipt: Box<Account<'info, DepositReceipt>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
    let vault = &ctx.accounts.vault;
    require!(!vault.paused_deposits, VaultError::DepositsPaused);
    require!(amount > 0, VaultError::ZeroAmount);
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.depositor_token.to_account_info(),
                to: ctx.accounts.pending.to_account_info(),
                authority: ctx.accounts.depositor.to_account_info(),
            },
        ),
        amount,
    )?;
    let round = vault.round + 1;
    let receipt = &mut ctx.accounts.receipt;
    receipt.owner = ctx.accounts.depositor.key();
    receipt.vault = vault.key();
    receipt.round = round;
    receipt.amount = amount;
    emit_cpi!(VaultDeposit {
        vault: vault.key(),
        depositor: ctx.accounts.depositor.key(),
        round,
        amount,
    });
    Ok(())
}

/// Convert a deposit receipt at `pps[round − 1]` (mirrors
/// `vault::claim_shares`). The shares were minted in aggregate at that
/// round's finalize; this just allocates them.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimShares<'info> {
    #[account(mut)]
    pub claimer: Signer<'info>,
    pub vault: Box<Account<'info, Vault>>,
    #[account(
        mut,
        close = claimer,
        constraint = receipt.owner == claimer.key() @ VaultError::ReceiptMismatch,
        constraint = receipt.vault == vault.key() @ VaultError::ReceiptMismatch,
    )]
    pub receipt: Box<Account<'info, DepositReceipt>>,
    #[account(
        seeds = [ROUND_SEED, vault.key().as_ref(), &(receipt.round - 1).to_le_bytes()],
        bump = round_state.bump,
    )]
    pub round_state: Box<Account<'info, RoundState>>,
    #[account(mut, seeds = [CLAIMABLE_SEED, vault.key().as_ref()], bump)]
    pub claimable_shares: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = vault.share_mint)]
    pub claimer_shares: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_claim_shares(ctx: Context<ClaimShares>) -> Result<()> {
    let vault = &ctx.accounts.vault;
    let receipt = &ctx.accounts.receipt;
    let pps = ctx.accounts.round_state.pps;
    let shares =
        options_math::shares_for_amount(receipt.amount, pps).ok_or(VaultError::MathOverflow)?;
    vault_seeds!(vault, salt, bump, seeds, signer_seeds);
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.claimable_shares.to_account_info(),
                to: ctx.accounts.claimer_shares.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            signer_seeds,
        ),
        shares,
    )?;
    emit_cpi!(SharesClaimed {
        vault: vault.key(),
        claimer: ctx.accounts.claimer.key(),
        round: receipt.round,
        amount: receipt.amount,
        shares,
    });
    Ok(())
}

/// Escrow shares for a two-step withdrawal (mirrors
/// `vault::initiate_withdraw`). The escrowed shares stay exposed to the
/// current round's P&L — Ribbon semantics.
#[event_cpi]
#[derive(Accounts)]
pub struct InitiateWithdraw<'info> {
    #[account(mut)]
    pub withdrawer: Signer<'info>,
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, seeds = [QUEUED_SEED, vault.key().as_ref()], bump)]
    pub queued_shares: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = vault.share_mint)]
    pub withdrawer_shares: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = withdrawer,
        space = 8 + WithdrawReceipt::INIT_SPACE,
        signer
    )]
    pub receipt: Box<Account<'info, WithdrawReceipt>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initiate_withdraw(ctx: Context<InitiateWithdraw>, shares: u64) -> Result<()> {
    require!(shares > 0, VaultError::ZeroAmount);
    let vault = &ctx.accounts.vault;
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.withdrawer_shares.to_account_info(),
                to: ctx.accounts.queued_shares.to_account_info(),
                authority: ctx.accounts.withdrawer.to_account_info(),
            },
        ),
        shares,
    )?;
    let receipt = &mut ctx.accounts.receipt;
    receipt.owner = ctx.accounts.withdrawer.key();
    receipt.vault = vault.key();
    receipt.round = vault.round;
    receipt.shares = shares;
    emit_cpi!(WithdrawInitiated {
        vault: vault.key(),
        withdrawer: ctx.accounts.withdrawer.key(),
        round: vault.round,
        shares,
    });
    Ok(())
}

/// Pay out a finalized withdrawal at its locked pps (mirrors
/// `vault::complete_withdraw`), from the pool finalize fully funded.
#[event_cpi]
#[derive(Accounts)]
pub struct CompleteWithdraw<'info> {
    #[account(mut)]
    pub withdrawer: Signer<'info>,
    pub vault: Box<Account<'info, Vault>>,
    #[account(
        mut,
        close = withdrawer,
        constraint = receipt.owner == withdrawer.key() @ VaultError::ReceiptMismatch,
        constraint = receipt.vault == vault.key() @ VaultError::ReceiptMismatch,
    )]
    pub receipt: Box<Account<'info, WithdrawReceipt>>,
    #[account(
        seeds = [ROUND_SEED, vault.key().as_ref(), &receipt.round.to_le_bytes()],
        bump = round_state.bump,
    )]
    pub round_state: Box<Account<'info, RoundState>>,
    #[account(mut, seeds = [WITHDRAWAL_SEED, vault.key().as_ref()], bump)]
    pub withdrawal_pool: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = vault.underlying_mint)]
    pub withdrawer_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_complete_withdraw(ctx: Context<CompleteWithdraw>) -> Result<()> {
    let vault = &ctx.accounts.vault;
    let receipt = &ctx.accounts.receipt;
    let pps = ctx.accounts.round_state.pps;
    let owed =
        options_math::amount_for_shares(receipt.shares, pps).ok_or(VaultError::MathOverflow)?;
    vault_seeds!(vault, salt, bump, seeds, signer_seeds);
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.withdrawal_pool.to_account_info(),
                to: ctx.accounts.withdrawer_token.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            signer_seeds,
        ),
        owed,
    )?;
    emit_cpi!(WithdrawCompleted {
        vault: vault.key(),
        withdrawer: ctx.accounts.withdrawer.key(),
        round: receipt.round,
        shares: receipt.shares,
        amount: owed,
    });
    Ok(())
}

/// Cancel a deposit whose round hasn't started (mirrors
/// `vault::instant_withdraw_pending`).
#[event_cpi]
#[derive(Accounts)]
pub struct InstantWithdrawPending<'info> {
    #[account(mut)]
    pub withdrawer: Signer<'info>,
    pub vault: Box<Account<'info, Vault>>,
    #[account(
        mut,
        close = withdrawer,
        constraint = receipt.owner == withdrawer.key() @ VaultError::ReceiptMismatch,
        constraint = receipt.vault == vault.key() @ VaultError::ReceiptMismatch,
        constraint = receipt.round > vault.round @ VaultError::ReceiptMismatch,
    )]
    pub receipt: Box<Account<'info, DepositReceipt>>,
    #[account(mut, seeds = [PENDING_SEED, vault.key().as_ref()], bump)]
    pub pending: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = vault.underlying_mint)]
    pub withdrawer_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_instant_withdraw_pending(ctx: Context<InstantWithdrawPending>) -> Result<()> {
    let vault = &ctx.accounts.vault;
    let receipt = &ctx.accounts.receipt;
    vault_seeds!(vault, salt, bump, seeds, signer_seeds);
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.pending.to_account_info(),
                to: ctx.accounts.withdrawer_token.to_account_info(),
                authority: ctx.accounts.vault.to_account_info(),
            },
            signer_seeds,
        ),
        receipt.amount,
    )?;
    emit_cpi!(InstantWithdraw {
        vault: vault.key(),
        withdrawer: ctx.accounts.withdrawer.key(),
        round: vault.round,
        amount: receipt.amount,
    });
    Ok(())
}
