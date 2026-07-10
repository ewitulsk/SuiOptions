use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{
    self, spl_token::instruction::AuthorityType, Mint, Token, TokenAccount,
};

use crate::error::CoreError;
use crate::events::*;
use crate::state::*;
use crate::util::now_ms;

/// Bucket PDA seeds are (pair, salt): the market parameters live in account
/// data, and the scheduler picks a fresh salt per bucket. Sui allowed
/// duplicate (pair, expiry, strike) buckets freely; the salt preserves that
/// flexibility (port plan decision #4) while keeping signer seeds short.
pub fn bucket_signer_seeds<'a>(
    underlying_mint: &'a Pubkey,
    settlement_mint: &'a Pubkey,
    salt: &'a [u8; 8],
    bump: &'a [u8; 1],
) -> [&'a [u8]; 5] {
    [
        BUCKET_SEED,
        underlying_mint.as_ref(),
        settlement_mint.as_ref(),
        salt,
        bump,
    ]
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(salt: u64)]
pub struct CreateBucket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Account<'info, Config>,
    #[account(constraint = underlying_mint.key() != settlement_mint.key() @ CoreError::AmountMismatch)]
    pub underlying_mint: Box<Account<'info, Mint>>,
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        space = 8 + Bucket::INIT_SPACE,
        seeds = [
            BUCKET_SEED,
            underlying_mint.key().as_ref(),
            settlement_mint.key().as_ref(),
            &salt.to_le_bytes(),
        ],
        bump
    )]
    pub bucket: Account<'info, Bucket>,
    /// The per-bucket option coin. Sui's per-roll OTW package publish +
    /// `TreasuryCap` handoff collapses into creating a fresh mint here with
    /// the bucket PDA as sole authority — zero supply by construction, so
    /// the supply == outstanding-options invariant holds from genesis.
    #[account(
        init,
        payer = admin,
        seeds = [CALL_MINT_SEED, bucket.key().as_ref()],
        bump,
        mint::decimals = underlying_mint.decimals,
        mint::authority = bucket,
    )]
    pub call_mint: Box<Account<'info, Mint>>,
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

pub fn handle_create_bucket(
    ctx: Context<CreateBucket>,
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
    bucket.call_mint = ctx.accounts.call_mint.key();
    bucket.expiry_ms = expiry_ms;
    bucket.strike = strike;
    bucket.strike_scale = strike_scale;
    bucket.total_written = 0;
    bucket.exercise_cursor = 0;
    bucket.invalidated = false;
    bucket.salt = salt;
    bucket.bump = ctx.bumps.bucket;
    emit_cpi!(BucketCreated {
        bucket: bucket.key(),
        underlying_mint: bucket.underlying_mint,
        settlement_mint: bucket.settlement_mint,
        call_mint: bucket.call_mint,
        expiry_ms,
        strike,
        strike_scale,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct ToggleBucketValidity<'info> {
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub bucket: Account<'info, Bucket>,
}

pub fn handle_invalidate_bucket(
    ctx: Context<ToggleBucketValidity>,
    reason: String,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &mut ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(!bucket.invalidated, CoreError::BucketInvalidated);
    bucket.invalidated = true;
    emit_cpi!(BucketInvalidated {
        bucket: bucket.key(),
        timestamp_ms: now,
        admin: ctx.accounts.admin.key(),
        reason,
    });
    Ok(())
}

pub fn handle_revalidate_bucket(
    ctx: Context<ToggleBucketValidity>,
    reason: String,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &mut ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(bucket.invalidated, CoreError::BucketNotInvalidated);
    bucket.invalidated = false;
    emit_cpi!(BucketRevalidated {
        bucket: bucket.key(),
        timestamp_ms: now,
        admin: ctx.accounts.admin.key(),
        reason,
    });
    Ok(())
}

/// Destroy an expired, fully-drained bucket. Mirrors `bucket::cleanup_bucket`:
/// both vaults must be empty; the vaults and the bucket account close with
/// rent to the admin, and the call-mint authority is handed to the admin —
/// the exact analog of transferring the `TreasuryCap` back (outstanding
/// option coins may still exist, so supply can't be forced to zero and a
/// mint cannot be closed).
#[event_cpi]
#[derive(Accounts)]
pub struct CleanupBucket<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        constraint = config.admin == admin.key() @ CoreError::NotOwner
    )]
    pub config: Account<'info, Config>,
    #[account(mut, close = admin)]
    pub bucket: Account<'info, Bucket>,
    #[account(mut, address = bucket.call_mint)]
    pub call_mint: Box<Account<'info, Mint>>,
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
    pub token_program: Program<'info, Token>,
}

pub fn handle_cleanup_bucket(ctx: Context<CleanupBucket>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    require!(now >= bucket.expiry_ms, CoreError::BucketNotExpired);
    require!(
        ctx.accounts.underlying_vault.amount == 0,
        CoreError::BucketNotDrained
    );
    require!(
        ctx.accounts.settlement_vault.amount == 0,
        CoreError::BucketNotDrained
    );

    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = bucket_signer_seeds(
        &bucket.underlying_mint,
        &bucket.settlement_mint,
        &salt,
        &bump,
    );
    let signer_seeds: &[&[&[u8]]] = &[&seeds];

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
                account_or_mint: ctx.accounts.call_mint.to_account_info(),
                current_authority: ctx.accounts.bucket.to_account_info(),
            },
            signer_seeds,
        ),
        AuthorityType::MintTokens,
        Some(ctx.accounts.admin.key()),
    )?;

    emit_cpi!(BucketCleaned {
        bucket: bucket.key()
    });
    Ok(())
}
