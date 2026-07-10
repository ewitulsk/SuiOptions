use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::instructions::put_admin::put_bucket_signer_seeds;
use crate::quote::{verify_ed25519_quote_ix, FlowKind, Quote};
use crate::state::*;
use crate::util::now_ms;

/// Cash collateral required to write `amount` underlying-units of a put:
/// ceil(amount × strike) — rounding UP is what makes the bucket provably
/// solvent (see the solvency proof in `put_bucket.move`).
pub fn required_collateral(bucket: &PutBucket, amount: u64) -> Result<u64> {
    options_math::apply_strike_ceil(amount as u128, bucket.strike, bucket.strike_scale)
        .ok_or_else(|| CoreError::MathOverflow.into())
}

/// Core cash-secured write (mirrors `put_bucket::write_collateralized`):
/// escrow the cash collateral, advance the cursor by `write_amount`
/// (underlying units — NOT the collateral value), mint Position + puts.
/// `payer` (rent) is separate from `writer` (collateral authority) so a
/// program PDA can be the writer under CPI; direct users pass the same
/// wallet for both.
#[event_cpi]
#[derive(Accounts)]
pub struct WritePutCollateralized<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub writer: Signer<'info>,
    #[account(mut)]
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(
        init,
        payer = payer,
        space = 8 + Position::INIT_SPACE,
        signer
    )]
    pub position: Box<Account<'info, Position>>,
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub writer_settlement: Box<Account<'info, TokenAccount>>,
    #[account(
        mut,
        associated_token::mint = bucket.settlement_mint,
        associated_token::authority = bucket,
    )]
    pub settlement_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = bucket.put_mint)]
    pub put_mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = put_mint)]
    pub put_dest: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_write_put_collateralized(
    ctx: Context<WritePutCollateralized>,
    write_amount: u64,
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
    require!(write_amount > 0, CoreError::ZeroAmount);

    let collateral = required_collateral(&ctx.accounts.bucket, write_amount)?;
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.writer_settlement.to_account_info(),
                to: ctx.accounts.settlement_vault.to_account_info(),
                authority: ctx.accounts.writer.to_account_info(),
            },
        ),
        collateral,
    )?;

    let (range_start, range_end) = put_do_write(
        &mut ctx.accounts.bucket,
        &mut ctx.accounts.position,
        position_owner,
        write_amount,
    );
    mint_puts(
        &ctx.accounts.bucket,
        &ctx.accounts.put_mint,
        &ctx.accounts.put_dest,
        &ctx.accounts.token_program,
        write_amount,
    )?;

    emit_cpi!(PutCollateralizedWrite {
        bucket: ctx.accounts.bucket.key(),
        writer: ctx.accounts.writer.key(),
        position: ctx.accounts.position.key(),
        write_amount,
        collateral,
        range_start,
        range_end,
    });
    Ok(())
}

/// Cursor + position bookkeeping shared by both put write paths (the
/// `put_bucket::do_write` analog). The cursor advances in UNDERLYING
/// units.
pub(crate) fn put_do_write(
    bucket: &mut Account<PutBucket>,
    position: &mut Account<Position>,
    position_owner: Pubkey,
    write_amount: u64,
) -> (u128, u128) {
    let range_start = bucket.total_written;
    let range_end = range_start + write_amount as u128;
    bucket.total_written = range_end;
    position.owner = position_owner;
    position.bucket = bucket.key();
    position.range_start = range_start;
    position.range_end = range_end;
    (range_start, range_end)
}

pub(crate) fn mint_puts<'info>(
    bucket: &Account<'info, PutBucket>,
    put_mint: &Account<'info, Mint>,
    put_dest: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    amount: u64,
) -> Result<()> {
    let _ = token_program;
    let salt = bucket.salt.to_le_bytes();
    let bump = [bucket.bump];
    let seeds = put_bucket_signer_seeds(
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
                mint: put_mint.to_account_info(),
                to: put_dest.to_account_info(),
                authority: bucket.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
    )
}

/// Quote-based put write (mirrors `put_bucket::execute_write`). Both the
/// collateral and the premium legs are settlement-mint, which is the only
/// structural difference from the call-side `execute_write`.
#[event_cpi]
#[derive(Accounts)]
#[instruction(quote: Quote)]
pub struct ExecutePutWrite<'info> {
    #[account(mut)]
    pub executor: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [TREASURY_SEED], bump = treasury.bump)]
    pub treasury: Box<Account<'info, Treasury>>,
    #[account(mut)]
    pub bucket: Box<Account<'info, PutBucket>>,
    #[account(address = bucket.settlement_mint)]
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = bucket.settlement_mint,
        associated_token::authority = bucket,
    )]
    pub settlement_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = bucket.put_mint)]
    pub put_mint: Box<Account<'info, Mint>>,
    /// Writer flow: must be owned by `quote.signer_token_recipient` (the
    /// buying MM); trader flow: the executor's choice.
    #[account(mut, token::mint = put_mint)]
    pub put_dest: Box<Account<'info, TokenAccount>>,
    pub mm_account: Box<Account<'info, MmAccount>>,
    /// Premium source (writer flow) / net-premium + collateral source
    /// (trader flow) — the MM account's settlement ATA covers both legs.
    #[account(
        mut,
        associated_token::mint = bucket.settlement_mint,
        associated_token::authority = mm_account,
    )]
    pub mm_settlement: Box<Account<'info, TokenAccount>>,
    /// Writer flow: pays the cash collateral. Trader flow: pays the gross
    /// premium.
    #[account(mut, token::mint = bucket.settlement_mint)]
    pub executor_settlement: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = executor,
        associated_token::mint = settlement_mint,
        associated_token::authority = treasury,
    )]
    pub treasury_settlement: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = executor,
        space = 8 + Position::INIT_SPACE,
        signer
    )]
    pub position: Box<Account<'info, Position>>,
    #[account(
        init,
        payer = executor,
        space = 8 + NonceRecord::INIT_SPACE,
        seeds = [NONCE_SEED, mm_account.key().as_ref(), &quote.nonce.to_le_bytes()],
        bump
    )]
    pub nonce_record: Box<Account<'info, NonceRecord>>,
    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = solana_sdk_ids::sysvar::instructions::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_execute_put_write(
    ctx: Context<ExecutePutWrite>,
    quote: Quote,
    flow: FlowKind,
    position_recipient: Pubkey,
    sig_ix_index: u8,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    let mm = &ctx.accounts.mm_account;

    require!(
        quote.protocol_id == ctx.accounts.config.key(),
        CoreError::QuoteProtocolMismatch
    );
    require!(
        quote.signer_account == mm.key(),
        CoreError::QuoteAccountMismatch
    );
    require!(now < quote.valid_until_ms, CoreError::QuoteExpired);
    require!(quote.bucket == bucket.key(), CoreError::QuoteBucketMismatch);

    let mut quote_bytes = Vec::with_capacity(176);
    quote.serialize(&mut quote_bytes)?;
    verify_ed25519_quote_ix(
        &ctx.accounts.instructions_sysvar,
        sig_ix_index,
        &mm.signing_pubkey,
        &quote_bytes,
    )?;

    let nonce_record = &mut ctx.accounts.nonce_record;
    nonce_record.mm_account = mm.key();
    nonce_record.nonce = quote.nonce;
    nonce_record.valid_until_ms = quote.valid_until_ms;
    nonce_record.bump = ctx.bumps.nonce_record;

    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(!bucket.invalidated, CoreError::BucketInvalidated);
    require!(quote.write_amount > 0, CoreError::ZeroAmount);

    let collateral = required_collateral(bucket, quote.write_amount)?;
    let gross_premium = quote.premium;
    let fee = options_math::fee_amount(gross_premium, ctx.accounts.config.fee_bps);
    let net_premium = gross_premium - fee;

    let mm_salt = mm.salt.to_le_bytes();
    let mm_bump = [mm.bump];
    let mm_owner = mm.owner;
    let mm_seeds: [&[u8]; 4] = [MM_ACCOUNT_SEED, mm_owner.as_ref(), &mm_salt, &mm_bump];
    let mm_signer: &[&[&[u8]]] = &[&mm_seeds];

    match flow {
        FlowKind::Writer => {
            // Signer is the trader MM — the BUYER of the put. Executor
            // (the writer) posts the cash collateral and keeps the net
            // premium; the MM gets the put coins.
            require!(
                ctx.accounts.put_dest.owner == quote.signer_token_recipient,
                CoreError::QuoteRecipientMismatch
            );
            require!(
                ctx.accounts.mm_settlement.amount >= gross_premium,
                CoreError::InsufficientAccountBalance
            );
            // Collateral: executor → bucket vault.
            token::transfer(
                CpiContext::new(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.executor_settlement.to_account_info(),
                        to: ctx.accounts.settlement_vault.to_account_info(),
                        authority: ctx.accounts.executor.to_account_info(),
                    },
                ),
                collateral,
            )?;
            // Fee: MM account → treasury.
            if fee > 0 {
                token::transfer(
                    CpiContext::new_with_signer(
                        token::ID,
                        token::Transfer {
                            from: ctx.accounts.mm_settlement.to_account_info(),
                            to: ctx.accounts.treasury_settlement.to_account_info(),
                            authority: ctx.accounts.mm_account.to_account_info(),
                        },
                        mm_signer,
                    ),
                    fee,
                )?;
            }
            // Net premium: MM account → executor.
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.mm_settlement.to_account_info(),
                        to: ctx.accounts.executor_settlement.to_account_info(),
                        authority: ctx.accounts.mm_account.to_account_info(),
                    },
                    mm_signer,
                ),
                net_premium,
            )?;
        }
        FlowKind::Trader => {
            // Signer is the writer MM — the SELLER of the put. Their
            // account posts the collateral and keeps the net premium; the
            // executor (the buyer) pays premium and gets the put coins.
            require!(
                position_recipient == quote.signer_token_recipient,
                CoreError::QuoteRecipientMismatch
            );
            require!(
                ctx.accounts.mm_settlement.amount >= collateral,
                CoreError::InsufficientAccountBalance
            );
            // Collateral: MM account → bucket vault.
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.mm_settlement.to_account_info(),
                        to: ctx.accounts.settlement_vault.to_account_info(),
                        authority: ctx.accounts.mm_account.to_account_info(),
                    },
                    mm_signer,
                ),
                collateral,
            )?;
            // Fee: executor → treasury.
            if fee > 0 {
                token::transfer(
                    CpiContext::new(
                        token::ID,
                        token::Transfer {
                            from: ctx.accounts.executor_settlement.to_account_info(),
                            to: ctx.accounts.treasury_settlement.to_account_info(),
                            authority: ctx.accounts.executor.to_account_info(),
                        },
                    ),
                    fee,
                )?;
            }
            // Net premium: executor → MM account.
            token::transfer(
                CpiContext::new(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.executor_settlement.to_account_info(),
                        to: ctx.accounts.mm_settlement.to_account_info(),
                        authority: ctx.accounts.executor.to_account_info(),
                    },
                ),
                net_premium,
            )?;
        }
    }

    let (range_start, range_end) = put_do_write(
        &mut ctx.accounts.bucket,
        &mut ctx.accounts.position,
        position_recipient,
        quote.write_amount,
    );
    mint_puts(
        &ctx.accounts.bucket,
        &ctx.accounts.put_mint,
        &ctx.accounts.put_dest,
        &ctx.accounts.token_program,
        quote.write_amount,
    )?;

    emit_cpi!(PutWriteExecuted {
        bucket: ctx.accounts.bucket.key(),
        signer_account: ctx.accounts.mm_account.key(),
        signer_token_recipient: quote.signer_token_recipient,
        executor: ctx.accounts.executor.key(),
        position: ctx.accounts.position.key(),
        position_recipient,
        put_token_recipient: ctx.accounts.put_dest.owner,
        write_amount: quote.write_amount,
        collateral,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        nonce: quote.nonce,
    });
    Ok(())
}
