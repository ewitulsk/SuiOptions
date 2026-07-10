use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::state::*;

#[event_cpi]
#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = admin,
        space = 8 + Treasury::INIT_SPACE,
        seeds = [TREASURY_SEED],
        bump
    )]
    pub treasury: Account<'info, Treasury>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize(ctx: Context<Initialize>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    config.admin = ctx.accounts.admin.key();
    config.fee_bps = 0;
    config.bump = ctx.bumps.config;
    ctx.accounts.treasury.bump = ctx.bumps.treasury;
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Account<'info, Config>,
}

pub fn handle_set_fee_bps(ctx: Context<AdminConfig>, new_bps: u64) -> Result<()> {
    require!(new_bps <= MAX_FEE_BPS, CoreError::FeeTooHigh);
    let old_bps = ctx.accounts.config.fee_bps;
    ctx.accounts.config.fee_bps = new_bps;
    emit_cpi!(FeeUpdated { old_bps, new_bps });
    Ok(())
}

/// The analog of transferring Sui's `AdminCap`.
pub fn handle_set_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
    let old_admin = ctx.accounts.config.admin;
    ctx.accounts.config.admin = new_admin;
    emit_cpi!(AdminChanged {
        old_admin,
        new_admin
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawTreasury<'info> {
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Account<'info, Config>,
    #[account(seeds = [TREASURY_SEED], bump = treasury.bump)]
    pub treasury: Account<'info, Treasury>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = treasury,
    )]
    pub treasury_token: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = mint)]
    pub recipient_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_withdraw_treasury(ctx: Context<WithdrawTreasury>, amount: u64) -> Result<()> {
    require!(
        ctx.accounts.treasury_token.amount >= amount,
        CoreError::InsufficientTreasuryBalance
    );
    let bump = [ctx.accounts.treasury.bump];
    let signer_seeds: &[&[&[u8]]] = &[&[TREASURY_SEED, &bump]];
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.treasury_token.to_account_info(),
                to: ctx.accounts.recipient_token.to_account_info(),
                authority: ctx.accounts.treasury.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
    )?;
    emit_cpi!(TreasuryWithdrawn {
        mint: ctx.accounts.mint.key(),
        amount,
        recipient: ctx.accounts.recipient_token.owner,
    });
    Ok(())
}

/// Permissionless deposit into the protocol treasury. This is how external
/// venues (audit package 2+) route the protocol fee without core knowing
/// they exist; anyone being able to PAY the treasury is harmless.
#[event_cpi]
#[derive(Accounts)]
pub struct DepositProtocolFee<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [TREASURY_SEED], bump = treasury.bump)]
    pub treasury: Account<'info, Treasury>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = mint)]
    pub from_token: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint,
        associated_token::authority = treasury,
    )]
    pub treasury_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_deposit_protocol_fee(ctx: Context<DepositProtocolFee>, amount: u64) -> Result<()> {
    require!(amount > 0, CoreError::ZeroAmount);
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.from_token.to_account_info(),
                to: ctx.accounts.treasury_token.to_account_info(),
                authority: ctx.accounts.payer.to_account_info(),
            },
        ),
        amount,
    )?;
    emit_cpi!(ProtocolFeeDeposited {
        mint: ctx.accounts.mint.key(),
        amount,
        payer: ctx.accounts.payer.key(),
    });
    Ok(())
}
