use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::VenueError;
use crate::events::*;
use crate::state::*;
use crate::util::now_ms;

/// Common creation parameters (the long Move arg lists, grouped).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct AuctionParams {
    pub reserve_bid: u64,
    pub duration_ms: u64,
    pub snipe_window_ms: u64,
    pub snipe_extension_ms: u64,
    pub max_extension_ms: u64,
    pub min_increment_bps: u64,
    /// Owner of the minted Position (adapter modes; ignored for swaps).
    pub position_recipient: Pubkey,
    /// Coupled-venue settle gate (None ⇒ permissionless settle).
    pub settle_authority: Option<Pubkey>,
}

/// `payer` (rent) is separate from `creator` (escrow authority + PDA
/// seed) so a program PDA can be the creator under CPI — PDAs owned by
/// another program cannot fund account creation. Direct users pass the
/// same wallet for both.
#[event_cpi]
#[derive(Accounts)]
#[instruction(salt: u64)]
pub struct CreateAuction<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub creator: Signer<'info>,
    pub escrow_mint: Box<Account<'info, Mint>>,
    pub bid_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = payer,
        space = 8 + Auction::INIT_SPACE,
        seeds = [AUCTION_SEED, creator.key().as_ref(), &salt.to_le_bytes()],
        bump
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        init,
        payer = payer,
        seeds = [ESCROW_SEED, auction.key().as_ref()],
        bump,
        token::mint = escrow_mint,
        token::authority = auction,
    )]
    pub escrow_vault: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = payer,
        seeds = [BIDS_SEED, auction.key().as_ref()],
        bump,
        token::mint = bid_mint,
        token::authority = auction,
    )]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    /// The creator's source of the escrowed tokens.
    #[account(mut, token::mint = escrow_mint)]
    pub escrow_source: Box<Account<'info, TokenAccount>>,
    /// Receives the winning bid at settle (net premium for adapters).
    /// Fixed here so coupled venues absorb into their own vaults.
    #[account(token::mint = bid_mint)]
    pub proceeds_token: Box<Account<'info, TokenAccount>>,
    /// Receives the escrow back on no-winner / refund / expired paths.
    #[account(token::mint = escrow_mint)]
    pub refund_token: Box<Account<'info, TokenAccount>>,
    /// Adapter modes: the options_core bucket this auction writes into.
    /// CHECK: deserialized and validated per-mode in the handler; unused
    /// for pure swaps (pass the auction itself as a placeholder).
    pub bucket: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

/// Shared body: validate timing, escrow, record state, emit.
#[allow(clippy::too_many_arguments)]
fn create_common(
    ctx: Context<CreateAuction>,
    salt: u64,
    mode: AuctionMode,
    bucket: Pubkey,
    escrow_amount: u64,
    notional: u64,
    params: &AuctionParams,
    now: u64,
) -> Result<()> {
    require!(escrow_amount > 0, VenueError::ZeroAmount);
    require!(
        params.duration_ms >= MIN_DURATION_MS,
        VenueError::DurationTooShort
    );
    let deadline_ms = now + params.duration_ms;
    let max_deadline_ms = deadline_ms + params.max_extension_ms;

    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.escrow_source.to_account_info(),
                to: ctx.accounts.escrow_vault.to_account_info(),
                authority: ctx.accounts.creator.to_account_info(),
            },
        ),
        escrow_amount,
    )?;

    let auction = &mut ctx.accounts.auction;
    auction.creator = ctx.accounts.creator.key();
    auction.salt = salt;
    auction.mode = mode;
    auction.bucket = bucket;
    auction.escrow_mint = ctx.accounts.escrow_mint.key();
    auction.bid_mint = ctx.accounts.bid_mint.key();
    auction.amount = escrow_amount;
    auction.notional = notional;
    auction.reserve_bid = params.reserve_bid;
    auction.deadline_ms = deadline_ms;
    auction.snipe_window_ms = params.snipe_window_ms;
    auction.snipe_extension_ms = params.snipe_extension_ms;
    auction.max_deadline_ms = max_deadline_ms;
    auction.min_increment_bps = params.min_increment_bps;
    auction.best_bidder = None;
    auction.best_token_recipient = None;
    auction.position_recipient = params.position_recipient;
    auction.proceeds_token = ctx.accounts.proceeds_token.key();
    auction.refund_token = ctx.accounts.refund_token.key();
    auction.settle_authority = params.settle_authority;
    auction.bump = ctx.bumps.auction;

    emit_cpi!(AuctionCreated {
        auction: auction.key(),
        mode,
        bucket,
        creator: auction.creator,
        escrow_mint: auction.escrow_mint,
        bid_mint: auction.bid_mint,
        amount: escrow_amount,
        notional,
        reserve_bid: params.reserve_bid,
        deadline_ms,
        max_deadline_ms,
        min_increment_bps: params.min_increment_bps,
        settle_authority: params.settle_authority,
    });
    Ok(())
}

/// Pure swap auction — the standalone mode with zero options-protocol
/// dependency (subsumes `swap_auction.move`, generalized to any pair).
pub fn handle_create_swap_auction(
    ctx: Context<CreateAuction>,
    salt: u64,
    escrow_amount: u64,
    params: AuctionParams,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    create_common(
        ctx,
        salt,
        AuctionMode::Swap,
        Pubkey::default(),
        escrow_amount,
        0,
        &params,
        now,
    )
}

/// Covered-call auction (`rfq::create`): escrow underlying for a bucket
/// write; bids are settlement premium.
pub fn handle_create_call_auction(
    ctx: Context<CreateAuction>,
    salt: u64,
    escrow_amount: u64,
    params: AuctionParams,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let bucket = deserialize_bucket::<options_core::state::Bucket>(&ctx.accounts.bucket)?;
    require!(
        ctx.accounts.escrow_mint.key() == bucket.underlying_mint
            && ctx.accounts.bid_mint.key() == bucket.settlement_mint,
        VenueError::BucketMismatch
    );
    validate_bucket_timing(now, bucket.expiry_ms, bucket.invalidated, &params)?;
    let bucket_key = ctx.accounts.bucket.key();
    create_common(
        ctx,
        salt,
        AuctionMode::CoveredCall,
        bucket_key,
        escrow_amount,
        escrow_amount,
        &params,
        now,
    )
}

/// Cash-secured-put auction (`rfq_put::create`): escrow the put's exact
/// ceil collateral for `notional` underlying-units; bids are settlement
/// premium (both legs settlement mint).
pub fn handle_create_put_auction(
    ctx: Context<CreateAuction>,
    salt: u64,
    notional: u64,
    params: AuctionParams,
) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    require!(notional > 0, VenueError::ZeroAmount);
    let bucket = deserialize_bucket::<options_core::state::PutBucket>(&ctx.accounts.bucket)?;
    require!(
        ctx.accounts.escrow_mint.key() == bucket.settlement_mint
            && ctx.accounts.bid_mint.key() == bucket.settlement_mint,
        VenueError::BucketMismatch
    );
    validate_bucket_timing(now, bucket.expiry_ms, bucket.invalidated, &params)?;
    let collateral =
        options_math::apply_strike_ceil(notional as u128, bucket.strike, bucket.strike_scale)
            .ok_or(VenueError::MathOverflow)?;
    let bucket_key = ctx.accounts.bucket.key();
    create_common(
        ctx,
        salt,
        AuctionMode::CashSecuredPut,
        bucket_key,
        collateral,
        notional,
        &params,
        now,
    )
}

fn validate_bucket_timing(
    now: u64,
    expiry_ms: u64,
    invalidated: bool,
    params: &AuctionParams,
) -> Result<()> {
    require!(now < expiry_ms && !invalidated, VenueError::BucketExpiredOrInvalid);
    let max_deadline = now + params.duration_ms + params.max_extension_ms;
    require!(
        max_deadline + SETTLE_BUFFER_MS <= expiry_ms,
        VenueError::TooCloseToExpiry
    );
    Ok(())
}

/// Owner + discriminator-checked read of a core account from an
/// UncheckedAccount (Account<T> can't be used because the same field
/// serves two bucket types across modes).
pub fn deserialize_bucket<T: anchor_lang::AccountDeserialize + anchor_lang::Owner>(
    info: &UncheckedAccount,
) -> Result<T> {
    require!(
        *info.owner == options_core::ID && T::owner() == options_core::ID,
        VenueError::BucketMismatch
    );
    let data = info.try_borrow_data()?;
    T::try_deserialize(&mut &data[..]).map_err(Into::into)
}
