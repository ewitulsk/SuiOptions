use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::instructions::bucket_admin::bucket_signer_seeds;
use crate::state::*;
use crate::util::now_ms;

/// Core covered write (mirrors `bucket::write_collateralized`): escrow
/// underlying in the bucket, mint the `Position` + option coins. Safe to
/// expose permissionlessly — the caller fully collateralizes every option
/// unit minted, and until they part with the option coins they hold both
/// sides of the trade. This is the composability surface external venues
/// (audit package 2+) CPI into; `position_owner` is the CPI analog of Sui
/// returning the `Position` to the caller.
///
/// The `position` account is a fresh client-generated keypair (mirrors Sui
/// object ids) — no PDA derivation race when two writes land in one slot.
#[event_cpi]
#[derive(Accounts)]
pub struct WriteCollateralized<'info> {
    #[account(mut)]
    pub writer: Signer<'info>,
    #[account(mut)]
    pub bucket: Account<'info, Bucket>,
    #[account(
        init,
        payer = writer,
        space = 8 + Position::INIT_SPACE,
        signer
    )]
    pub position: Account<'info, Position>,
    #[account(mut, token::mint = bucket.underlying_mint)]
    pub writer_underlying: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        associated_token::mint = bucket.underlying_mint,
        associated_token::authority = bucket,
    )]
    pub underlying_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = bucket.call_mint)]
    pub call_mint: Box<Account<'info, Mint>>,
    /// Where the freshly minted option coins land; any token account of
    /// the bucket's call mint (typically the writer's ATA).
    #[account(mut, token::mint = call_mint)]
    pub call_dest: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_write_collateralized(
    ctx: Context<WriteCollateralized>,
    amount: u64,
    position_owner: Pubkey,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    require!(
        now < ctx.accounts.bucket.expiry_ms,
        CoreError::BucketExpired
    );
    require!(
        !ctx.accounts.bucket.invalidated,
        CoreError::BucketInvalidated
    );
    require!(amount > 0, CoreError::ZeroAmount);

    // Escrow the underlying.
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.writer_underlying.to_account_info(),
                to: ctx.accounts.underlying_vault.to_account_info(),
                authority: ctx.accounts.writer.to_account_info(),
            },
        ),
        amount,
    )?;

    // Advance the write cursor and mint the position over its range.
    let bucket = &mut ctx.accounts.bucket;
    let range_start = bucket.total_written;
    let range_end = range_start + amount as u128;
    bucket.total_written = range_end;

    let position = &mut ctx.accounts.position;
    position.owner = position_owner;
    position.bucket = bucket.key();
    position.range_start = range_start;
    position.range_end = range_end;

    // Mint the option coins from the bucket's own mint authority.
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
    token::mint_to(
        CpiContext::new_with_signer(
            token::ID,
            token::MintTo {
                mint: ctx.accounts.call_mint.to_account_info(),
                to: ctx.accounts.call_dest.to_account_info(),
                authority: ctx.accounts.bucket.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
    )?;

    emit_cpi!(CollateralizedWrite {
        bucket: bucket.key(),
        writer: ctx.accounts.writer.key(),
        position: ctx.accounts.position.key(),
        amount,
        range_start,
        range_end,
    });
    Ok(())
}
