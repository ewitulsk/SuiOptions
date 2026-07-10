use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{
    self, spl_token::instruction::AuthorityType, Mint, Token, TokenAccount,
};

use crate::error::CoreError;
use crate::events::*;
use crate::state::*;
use crate::util::now_ms;

pub fn put_bucket_signer_seeds<'a>(
    underlying_mint: &'a Pubkey,
    settlement_mint: &'a Pubkey,
    salt: &'a [u8; 8],
    bump: &'a [u8; 1],
) -> [&'a [u8]; 5] {
    [
        PUT_BUCKET_SEED,
        underlying_mint.as_ref(),
        settlement_mint.as_ref(),
        salt,
        bump,
    ]
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(salt: u64)]
pub struct CreatePutBucket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Box<Account<'info, Config>>,
    #[account(constraint = underlying_mint.key() != settlement_mint.key() @ CoreError::AmountMismatch)]
    pub underlying_mint: Box<Account<'info, Mint>>,
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        space = 8 + PutBucket::INIT_SPACE,
        seeds = [
            PUT_BUCKET_SEED,
            underlying_mint.key().as_ref(),
            settlement_mint.key().as_ref(),
            &salt.to_le_bytes(),
        ],
        bump
    )]
    pub bucket: Box<Account<'info, PutBucket>>,
    /// The per-bucket put coin: put units are denominated in UNDERLYING
    /// smallest-units (the option's notional), so decimals mirror the
    /// underlying, exactly like the call mint.
    #[account(
        init,
        payer = admin,
        seeds = [PUT_MINT_SEED, bucket.key().as_ref()],
        bump,
        mint::decimals = underlying_mint.decimals,
        mint::authority = bucket,
    )]
    pub put_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        associated_token::mint = underlying_mint,
        associated_token::authority = bucket,
    )]
    pub underlying_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = admin,
        associated_token::mint = settlement_mint,
        associated_token::authority = bucket,
    )]
    pub settlement_vault: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_create_put_bucket(
    ctx: Context<CreatePutBucket>,
    salt: u64,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
) -> Result<()> {
    require!(
        strike_scale <= options_math::MAX_STRIKE_SCALE,
        CoreError::StrikeScaleTooLarge
    );
    let bucket = &mut ctx.accounts.bucket;
    bucket.underlying_mint = ctx.accounts.underlying_mint.key();
    bucket.settlement_mint = ctx.accounts.settlement_mint.key();
    bucket.put_mint = ctx.accounts.put_mint.key();
    bucket.expiry_ms = expiry_ms;
    bucket.strike = strike;
    bucket.strike_scale = strike_scale;
    bucket.total_written = 0;
    bucket.exercise_cursor = 0;
    bucket.total_redeemed = 0;
    bucket.invalidated = false;
    bucket.salt = salt;
    bucket.bump = ctx.bumps.bucket;
    emit_cpi!(PutBucketCreated {
        bucket: bucket.key(),
        underlying_mint: bucket.underlying_mint,
        settlement_mint: bucket.settlement_mint,
        put_mint: bucket.put_mint,
        expiry_ms,
        strike,
        strike_scale,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct TogglePutBucketValidity<'info> {
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Box<Account<'info, Config>>,
    #[account(mut)]
    pub bucket: Box<Account<'info, PutBucket>>,
}

pub fn handle_invalidate_put_bucket(
    ctx: Context<TogglePutBucketValidity>,
    reason: String,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &mut ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(!bucket.invalidated, CoreError::BucketInvalidated);
    bucket.invalidated = true;
    emit_cpi!(PutBucketInvalidated {
        bucket: bucket.key(),
        timestamp_ms: now,
        admin: ctx.accounts.admin.key(),
        reason,
    });
    Ok(())
}

pub fn handle_revalidate_put_bucket(
    ctx: Context<TogglePutBucketValidity>,
    reason: String,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &mut ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(bucket.invalidated, CoreError::BucketNotInvalidated);
    bucket.invalidated = false;
    emit_cpi!(PutBucketRevalidated {
        bucket: bucket.key(),
        timestamp_ms: now,
        admin: ctx.accounts.admin.key(),
        reason,
    });
    Ok(())
}

/// Destroy an expired put bucket (mirrors `put_bucket::cleanup_bucket`).
/// The ceil-in/floor-out rounding leaves a non-negative cash dust
/// remainder, so the gate is `total_redeemed == total_written` (every
/// position redeemed — the dust can never be an unredeemed writer's
/// collateral) and the remainder is swept to the admin.
#[event_cpi]
#[derive(Accounts)]
pub struct CleanupPutBucket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Box<Account<'info, Config>>,
    #[account(mut, close = admin)]
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(mut, address = bucket.put_mint)]
    pub put_mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = bucket.underlying_mint,
        associated_token::authority = bucket,
    )]
    pub underlying_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        associated_token::mint = bucket.settlement_mint,
        associated_token::authority = bucket,
    )]
    pub settlement_vault: Box<Account<'info, TokenAccount>>,
    /// Receives the rounding-dust sweep.
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub admin_settlement: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_cleanup_put_bucket(ctx: Context<CleanupPutBucket>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    require!(now >= bucket.expiry_ms, CoreError::BucketNotExpired);
    require!(
        bucket.total_redeemed == bucket.total_written,
        CoreError::BucketNotDrained
    );
    require!(
        ctx.accounts.underlying_vault.amount == 0,
        CoreError::BucketNotDrained
    );

    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = put_bucket_signer_seeds(
        &bucket.underlying_mint,
        &bucket.settlement_mint,
        &salt,
        &bump,
    );
    let signer_seeds: &[&[&[u8]]] = &[&seeds];

    // Sweep the rounding remainder to the admin (Sui transferred the dust
    // coin to the sender).
    let dust = ctx.accounts.settlement_vault.amount;
    if dust > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.settlement_vault.to_account_info(),
                    to: ctx.accounts.admin_settlement.to_account_info(),
                    authority: ctx.accounts.bucket.to_account_info(),
                },
                signer_seeds,
            ),
            dust,
        )?;
    }
    token::close_account(CpiContext::new_with_signer(
        token::ID,
        token::CloseAccount {
            account: ctx.accounts.underlying_vault.to_account_info(),
            destination: ctx.accounts.admin.to_account_info(),
            authority: ctx.accounts.bucket.to_account_info(),
        },
        signer_seeds,
    ))?;
    token::close_account(CpiContext::new_with_signer(
        token::ID,
        token::CloseAccount {
            account: ctx.accounts.settlement_vault.to_account_info(),
            destination: ctx.accounts.admin.to_account_info(),
            authority: ctx.accounts.bucket.to_account_info(),
        },
        signer_seeds,
    ))?;
    token::set_authority(
        CpiContext::new_with_signer(
            token::ID,
            token::SetAuthority {
                account_or_mint: ctx.accounts.put_mint.to_account_info(),
                current_authority: ctx.accounts.bucket.to_account_info(),
            },
            signer_seeds,
        ),
        AuthorityType::MintTokens,
        Some(ctx.accounts.admin.key()),
    )?;

    emit_cpi!(PutBucketCleaned {
        bucket: bucket.key(),
        dust_swept: dust,
    });
    Ok(())
}
