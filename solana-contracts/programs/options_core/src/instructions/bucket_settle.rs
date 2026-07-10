use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::instructions::bucket_admin::bucket_signer_seeds;
use crate::state::*;
use crate::util::now_ms;

/// Exercise (mirrors `bucket::exercise`): burn option coins, pay
/// `round_half_up(amount × strike)` settlement in, receive `amount`
/// underlying out, cursor advances. The Sui "coin belongs to this bucket by
/// type" guarantee becomes the `token::mint = bucket.call_mint` constraint.
#[event_cpi]
#[derive(Accounts)]
pub struct Exercise<'info> {
    pub exerciser: Signer<'info>,
    #[account(mut)]
    pub bucket: Account<'info, Bucket>,
    #[account(mut, address = bucket.call_mint)]
    pub call_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = call_mint)]
    pub exerciser_call: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub exerciser_settlement: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = bucket.underlying_mint)]
    pub exerciser_underlying: Box<Account<'info, TokenAccount>>,
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

pub fn handle_exercise(ctx: Context<Exercise>, amount: u64) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(amount > 0, CoreError::ZeroAmount);

    let required_settlement =
        options_math::apply_strike(amount as u128, bucket.strike, bucket.strike_scale)
            .ok_or(CoreError::MathOverflow)?;
    require!(
        bucket.exercise_cursor + amount as u128 <= bucket.total_written,
        CoreError::CursorOverflow
    );

    // Burn through the bucket's own mint — the runtime analog of Sui's
    // type-level bucket isolation.
    token::burn(
        CpiContext::new(
            token::ID,
            token::Burn {
                mint: ctx.accounts.call_mint.to_account_info(),
                from: ctx.accounts.exerciser_call.to_account_info(),
                authority: ctx.accounts.exerciser.to_account_info(),
            },
        ),
        amount,
    )?;

    // Settlement in.
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.exerciser_settlement.to_account_info(),
                to: ctx.accounts.settlement_vault.to_account_info(),
                authority: ctx.accounts.exerciser.to_account_info(),
            },
        ),
        required_settlement,
    )?;

    let bucket = &mut ctx.accounts.bucket;
    bucket.exercise_cursor += amount as u128;
    let cursor_after = bucket.exercise_cursor;

    // Underlying out, signed by the bucket PDA.
    let bucket = &ctx.accounts.bucket;
    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = bucket_signer_seeds(
        &bucket.underlying_mint,
        &bucket.settlement_mint,
        &salt,
        &bump,
    );
    let signer_seeds: &[&[&[u8]]] = &[&seeds];
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.underlying_vault.to_account_info(),
                to: ctx.accounts.exerciser_underlying.to_account_info(),
                authority: ctx.accounts.bucket.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
    )?;

    emit_cpi!(Exercised {
        bucket: bucket.key(),
        exerciser: ctx.accounts.exerciser.key(),
        amount,
        settlement_paid: required_settlement,
        cursor_after,
    });
    Ok(())
}

/// Post-expiry redemption (mirrors `bucket::redeem_position`): the FIFO
/// cursor decides how much of the position's range was exercised; the
/// holder receives unexercised underlying + exercised × strike settlement.
/// The position account closes with rent to the redeemer.
#[event_cpi]
#[derive(Accounts)]
pub struct RedeemPosition<'info> {
    #[account(mut)]
    pub redeemer: Signer<'info>,
    #[account(mut)]
    pub bucket: Account<'info, Bucket>,
    #[account(
        mut,
        close = redeemer,
        constraint = position.owner == redeemer.key() @ CoreError::NotOwner,
        constraint = position.bucket == bucket.key() @ CoreError::PositionBucketMismatch,
    )]
    pub position: Account<'info, Position>,
    #[account(mut, token::mint = bucket.underlying_mint)]
    pub redeemer_underlying: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub redeemer_settlement: Box<Account<'info, TokenAccount>>,
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

pub fn handle_redeem_position(ctx: Context<RedeemPosition>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    let position = &ctx.accounts.position;
    require!(now >= bucket.expiry_ms, CoreError::BucketNotExpired);

    let (rs, re) = (position.range_start, position.range_end);
    let exercised = options_math::exercised_in_range(bucket.exercise_cursor, rs, re);
    let unexercised = (re - rs) - exercised;

    let underlying_amount = u64::try_from(unexercised).map_err(|_| CoreError::MathOverflow)?;
    let settlement_amount =
        options_math::apply_strike(exercised, bucket.strike, bucket.strike_scale)
            .ok_or(CoreError::MathOverflow)?;

    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = bucket_signer_seeds(
        &bucket.underlying_mint,
        &bucket.settlement_mint,
        &salt,
        &bump,
    );
    let signer_seeds: &[&[&[u8]]] = &[&seeds];
    if underlying_amount > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.underlying_vault.to_account_info(),
                    to: ctx.accounts.redeemer_underlying.to_account_info(),
                    authority: ctx.accounts.bucket.to_account_info(),
                },
                signer_seeds,
            ),
            underlying_amount,
        )?;
    }
    if settlement_amount > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.settlement_vault.to_account_info(),
                    to: ctx.accounts.redeemer_settlement.to_account_info(),
                    authority: ctx.accounts.bucket.to_account_info(),
                },
                signer_seeds,
            ),
            settlement_amount,
        )?;
    }

    emit_cpi!(Redeemed {
        bucket: bucket.key(),
        position: position.key(),
        redeemer: ctx.accounts.redeemer.key(),
        range_start: rs,
        range_end: re,
        underlying_returned: underlying_amount,
        settlement_returned: settlement_amount,
    });
    Ok(())
}

/// Burn worthless option coins after expiry (mirrors
/// `bucket::burn_expired_option`).
#[event_cpi]
#[derive(Accounts)]
pub struct BurnExpiredOption<'info> {
    pub burner: Signer<'info>,
    pub bucket: Account<'info, Bucket>,
    #[account(mut, address = bucket.call_mint)]
    pub call_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = call_mint)]
    pub burner_call: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_burn_expired_option(ctx: Context<BurnExpiredOption>, amount: u64) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    require!(
        now >= ctx.accounts.bucket.expiry_ms,
        CoreError::BucketNotExpired
    );
    token::burn(
        CpiContext::new(
            token::ID,
            token::Burn {
                mint: ctx.accounts.call_mint.to_account_info(),
                from: ctx.accounts.burner_call.to_account_info(),
                authority: ctx.accounts.burner.to_account_info(),
            },
        ),
        amount,
    )?;
    emit_cpi!(ExpiredOptionBurned {
        bucket: ctx.accounts.bucket.key(),
        burner: ctx.accounts.burner.key(),
        amount,
    });
    Ok(())
}

/// Preserves the Sui `Position`'s `key + store` transferability: the owner
/// reassigns the record to a new owner.
#[event_cpi]
#[derive(Accounts)]
pub struct TransferPosition<'info> {
    pub owner: Signer<'info>,
    #[account(
        mut,
        constraint = position.owner == owner.key() @ CoreError::NotOwner
    )]
    pub position: Account<'info, Position>,
}

pub fn handle_transfer_position(ctx: Context<TransferPosition>, new_owner: Pubkey) -> Result<()> {
    let old_owner = ctx.accounts.position.owner;
    ctx.accounts.position.owner = new_owner;
    emit_cpi!(PositionTransferred {
        position: ctx.accounts.position.key(),
        old_owner,
        new_owner,
    });
    Ok(())
}
