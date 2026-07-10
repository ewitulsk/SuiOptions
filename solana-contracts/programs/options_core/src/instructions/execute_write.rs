use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::instructions::bucket_admin::bucket_signer_seeds;
use crate::quote::{verify_ed25519_quote_ix, FlowKind, Quote};
use crate::state::*;
use crate::util::now_ms;

/// The unified quote-based write (mirrors `bucket::execute_write`): an
/// executor lands an MM-signed quote on-chain. Writer flow: the executor
/// escrows underlying and receives the net premium from the MM's account;
/// the MM receives the call coins. Trader flow: the executor pays premium
/// (net of fee to the MM's account) and receives the call coins; the MM's
/// account provides the underlying and the MM receives the `Position`.
///
/// Replay protection: the `nonce_record` PDA is created here — a nonce
/// that was already consumed makes the `init` fail before the handler
/// runs (the Sui `E_QUOTE_NONCE_USED` analog).
#[event_cpi]
#[derive(Accounts)]
#[instruction(quote: Quote)]
pub struct ExecuteWrite<'info> {
    #[account(mut)]
    pub executor: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,
    #[account(seeds = [TREASURY_SEED], bump = treasury.bump)]
    pub treasury: Box<Account<'info, Treasury>>,
    #[account(mut)]
    pub bucket: Box<Account<'info, Bucket>>,
    #[account(address = bucket.settlement_mint)]
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = bucket.underlying_mint,
        associated_token::authority = bucket,
    )]
    pub underlying_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = bucket.call_mint)]
    pub call_mint: Box<Account<'info, Mint>>,
    /// Destination of the minted option coins. Writer flow: must be owned
    /// by `quote.signer_token_recipient` (the buying MM); trader flow: the
    /// executor's choice.
    #[account(mut, token::mint = call_mint)]
    pub call_dest: Box<Account<'info, TokenAccount>>,
    /// The signing MM's account; bound to the quote by
    /// `quote.signer_account == mm_account.key()`.
    pub mm_account: Box<Account<'info, MmAccount>>,
    /// The MM account's settlement ATA: premium source (writer flow) or
    /// net-premium destination (trader flow).
    #[account(
        mut,
        associated_token::mint = bucket.settlement_mint,
        associated_token::authority = mm_account,
    )]
    pub mm_settlement: Box<Account<'info, TokenAccount>>,
    /// Trader flow only: the MM account's underlying ATA (collateral
    /// source).
    #[account(
        mut,
        associated_token::mint = bucket.underlying_mint,
        associated_token::authority = mm_account,
    )]
    pub mm_underlying: Option<Box<Account<'info, TokenAccount>>>,
    /// Writer flow only: the executor's underlying source.
    #[account(mut, token::mint = bucket.underlying_mint)]
    pub executor_underlying: Option<Box<Account<'info, TokenAccount>>>,
    /// Writer flow: receives the net premium. Trader flow: pays the gross
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
    /// CHECK: the Instructions sysvar, address-pinned; read via
    /// `load_instruction_at_checked` for precompile introspection.
    #[account(address = solana_sdk_ids::sysvar::instructions::ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_execute_write(
    ctx: Context<ExecuteWrite>,
    quote: Quote,
    flow: FlowKind,
    position_recipient: Pubkey,
    sig_ix_index: u8,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = &ctx.accounts.bucket;
    let mm = &ctx.accounts.mm_account;

    // ── verify_and_consume_quote (quote.move) ──
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

    // Record the consumed nonce (the PDA init above already rejected
    // replays); prune_nonce reclaims it after expiry.
    let nonce_record = &mut ctx.accounts.nonce_record;
    nonce_record.mm_account = mm.key();
    nonce_record.nonce = quote.nonce;
    nonce_record.valid_until_ms = quote.valid_until_ms;
    nonce_record.bump = ctx.bumps.nonce_record;

    // ── bucket checks (execute_write_with_quote) ──
    require!(now < bucket.expiry_ms, CoreError::BucketExpired);
    require!(!bucket.invalidated, CoreError::BucketInvalidated);
    require!(quote.write_amount > 0, CoreError::ZeroAmount);

    let gross_premium = quote.premium;
    let fee = options_math::fee_amount(gross_premium, ctx.accounts.config.fee_bps);
    let net_premium = gross_premium - fee;

    // MM account PDA signer seeds (it owns its ATAs).
    let mm_salt = mm.salt.to_le_bytes();
    let mm_bump = [mm.bump];
    let mm_owner = mm.owner;
    let mm_seeds: [&[u8]; 4] = [MM_ACCOUNT_SEED, mm_owner.as_ref(), &mm_salt, &mm_bump];
    let mm_signer: &[&[&[u8]]] = &[&mm_seeds];

    match flow {
        FlowKind::Writer => {
            // Signer is the trader MM (buyer): premium out of their
            // account, call coins to their recipient; the executor
            // provides the underlying and keeps the net premium.
            require!(
                ctx.accounts.call_dest.owner == quote.signer_token_recipient,
                CoreError::QuoteRecipientMismatch
            );
            let executor_underlying = ctx
                .accounts
                .executor_underlying
                .as_ref()
                .ok_or(CoreError::AmountMismatch)?;
            require!(
                ctx.accounts.mm_settlement.amount >= gross_premium,
                CoreError::InsufficientAccountBalance
            );

            // Underlying: executor → bucket vault.
            token::transfer(
                CpiContext::new(
                    token::ID,
                    token::Transfer {
                        from: executor_underlying.to_account_info(),
                        to: ctx.accounts.underlying_vault.to_account_info(),
                        authority: ctx.accounts.executor.to_account_info(),
                    },
                ),
                quote.write_amount,
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
            // Signer is the writer MM (seller): underlying out of their
            // account, Position to their recipient; the executor pays the
            // premium and receives the call coins.
            require!(
                position_recipient == quote.signer_token_recipient,
                CoreError::QuoteRecipientMismatch
            );
            let mm_underlying = ctx
                .accounts
                .mm_underlying
                .as_ref()
                .ok_or(CoreError::AmountMismatch)?;
            require!(
                mm_underlying.amount >= quote.write_amount,
                CoreError::InsufficientAccountBalance
            );

            // Underlying: MM account → bucket vault.
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: mm_underlying.to_account_info(),
                        to: ctx.accounts.underlying_vault.to_account_info(),
                        authority: ctx.accounts.mm_account.to_account_info(),
                    },
                    mm_signer,
                ),
                quote.write_amount,
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

    // ── cursor + mints (do_write) ──
    let bucket = &mut ctx.accounts.bucket;
    let range_start = bucket.total_written;
    let range_end = range_start + quote.write_amount as u128;
    bucket.total_written = range_end;

    let position = &mut ctx.accounts.position;
    position.owner = position_recipient;
    position.bucket = bucket.key();
    position.range_start = range_start;
    position.range_end = range_end;

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
        quote.write_amount,
    )?;

    emit_cpi!(WriteExecuted {
        bucket: bucket.key(),
        signer_account: ctx.accounts.mm_account.key(),
        signer_token_recipient: quote.signer_token_recipient,
        executor: ctx.accounts.executor.key(),
        position: ctx.accounts.position.key(),
        position_recipient,
        call_token_recipient: ctx.accounts.call_dest.owner,
        write_amount: quote.write_amount,
        gross_premium,
        fee,
        net_premium,
        range_start,
        range_end,
        nonce: quote.nonce,
    });
    Ok(())
}
