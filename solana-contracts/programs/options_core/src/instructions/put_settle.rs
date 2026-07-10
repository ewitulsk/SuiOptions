use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::instructions::put_admin::put_bucket_signer_seeds;
use crate::state::*;
use crate::util::now_ms;

/// Exercise puts (mirrors `put_bucket::exercise`): burn put coins, DELIVER
/// one underlying unit per put unit, receive floor(amount × strike) cash
/// out — floor is the solvency-preserving direction for cash payouts.
#[event_cpi]
#[derive(Accounts)]
pub struct ExercisePut<'info> {
    pub exerciser: Signer<'info>,
    #[account(mut)]
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(mut, address = bucket.put_mint)]
    pub put_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = put_mint)]
    pub exerciser_put: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = bucket.underlying_mint)]
    pub exerciser_underlying: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub exerciser_settlement: Box<Account<'info, TokenAccount>>,
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

pub fn handle_exercise_put(ctx: Context<ExercisePut>, amount: u64) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(amount > 0, CoreError::ZeroAmount);
    require!(
        bucket.exercise_cursor + amount as u128 <= bucket.total_written,
        CoreError::CursorOverflow
    );

    let payout = options_math::apply_strike_floor(amount as u128, bucket.strike, bucket.strike_scale)
        .ok_or(CoreError::MathOverflow)?;

    // Burn through the bucket's own mint (bucket isolation).
    token::burn(
        CpiContext::new(
            token::ID,
            token::Burn {
                mint: ctx.accounts.put_mint.to_account_info(),
                from: ctx.accounts.exerciser_put.to_account_info(),
                authority: ctx.accounts.exerciser.to_account_info(),
            },
        ),
        amount,
    )?;

    // Deliver the underlying (one unit per put unit).
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.exerciser_underlying.to_account_info(),
                to: ctx.accounts.underlying_vault.to_account_info(),
                authority: ctx.accounts.exerciser.to_account_info(),
            },
        ),
        amount,
    )?;

    let bucket = &mut ctx.accounts.bucket;
    bucket.exercise_cursor += amount as u128;
    let cursor_after = bucket.exercise_cursor;

    // Cash out, signed by the bucket PDA.
    let bucket = &ctx.accounts.bucket;
    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = put_bucket_signer_seeds(
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
                from: ctx.accounts.settlement_vault.to_account_info(),
                to: ctx.accounts.exerciser_settlement.to_account_info(),
                authority: ctx.accounts.bucket.to_account_info(),
            },
            signer_seeds,
        ),
        payout,
    )?;

    emit_cpi!(PutExercised {
        bucket: bucket.key(),
        exerciser: ctx.accounts.exerciser.key(),
        amount,
        settlement_paid: payout,
        cursor_after,
    });
    Ok(())
}

/// Redeem a put position after expiry (mirrors
/// `put_bucket::redeem_position`): the exercised range returns the
/// DELIVERED UNDERLYING; the unexercised range returns the writer's cash
/// collateral at floor(unexercised × strike).
#[event_cpi]
#[derive(Accounts)]
pub struct RedeemPutPosition<'info> {
    #[account(mut)]
    pub redeemer: Signer<'info>,
    #[account(mut)]
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(
        mut,
        close = redeemer,
        constraint = position.owner == redeemer.key() @ CoreError::NotOwner,
        constraint = position.bucket == bucket.key() @ CoreError::PositionBucketMismatch,
    )]
    pub position: Box<Account<'info, Position>>,
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

pub fn handle_redeem_put_position(ctx: Context<RedeemPutPosition>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    let position = &ctx.accounts.position;
    require!(now >= bucket.expiry_ms, CoreError::BucketNotExpired);

    let (rs, re) = (position.range_start, position.range_end);
    let exercised = options_math::exercised_in_range(bucket.exercise_cursor, rs, re);
    let total_range = re - rs;
    let unexercised = total_range - exercised;

    // Exercised range → the underlying holders delivered; unexercised
    // range → the writer's untouched cash collateral (floor).
    let underlying_amount = u64::try_from(exercised).map_err(|_| CoreError::MathOverflow)?;
    let settlement_amount =
        options_math::apply_strike_floor(unexercised, bucket.strike, bucket.strike_scale)
            .ok_or(CoreError::MathOverflow)?;

    let bucket = &mut ctx.accounts.bucket;
    bucket.total_redeemed += total_range;

    let bucket = &ctx.accounts.bucket;
    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = put_bucket_signer_seeds(
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

    emit_cpi!(PutRedeemed {
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

#[event_cpi]
#[derive(Accounts)]
pub struct BurnExpiredPut<'info> {
    pub burner: Signer<'info>,
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(mut, address = bucket.put_mint)]
    pub put_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = put_mint)]
    pub burner_put: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_burn_expired_put(ctx: Context<BurnExpiredPut>, amount: u64) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    require!(
        now >= ctx.accounts.bucket.expiry_ms,
        CoreError::BucketNotExpired
    );
    token::burn(
        CpiContext::new(
            token::ID,
            token::Burn {
                mint: ctx.accounts.put_mint.to_account_info(),
                from: ctx.accounts.burner_put.to_account_info(),
                authority: ctx.accounts.burner.to_account_info(),
            },
        ),
        amount,
    )?;
    emit_cpi!(PutExpiredOptionBurned {
        bucket: ctx.accounts.bucket.key(),
        burner: ctx.accounts.burner.key(),
        amount,
    });
    Ok(())
}
