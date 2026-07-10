use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use auction_venue::instructions::create::AuctionParams;

use crate::error::VaultError;
use crate::vault_seeds;
use crate::events::*;
use crate::oracle;
use crate::state::*;
use crate::util::now_ms;

/// The fresh-Pyth band floor for converting `amount_s` settlement: the
/// minimum underlying a fill must deliver = Pyth value × (1 − slippage)
/// (mirrors `vault::swap_floor`).
fn swap_floor(config: &VaultConfig, amount_s: u64, spot: u128, spot_scale: u8) -> Result<u64> {
    let u_fair = options_math::settlement_to_underlying(amount_s, spot, spot_scale)
        .ok_or(VaultError::MathOverflow)?;
    Ok(((u_fair as u128) * (options_math::BPS_DENOM - config.max_swap_slippage_bps as u128)
        / options_math::BPS_DENOM) as u64)
}

/// Escrow settlement proceeds into a coupled swap auction (mirrors
/// `vault::open_swap_rfq`). MMs bid underlying for it; the binding band
/// check is re-applied against a fresh cross at settle. Legal in any
/// phase.
#[event_cpi]
#[derive(Accounts)]
pub struct OpenSwapRfq<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(address = vault.underlying_mint)]
    pub underlying_mint: Box<Account<'info, Mint>>,
    #[account(address = vault.settlement_mint)]
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub underlying_price: UncheckedAccount<'info>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub settlement_price: UncheckedAccount<'info>,
    /// CHECK: venue init.
    #[account(mut)]
    pub auction: UncheckedAccount<'info>,
    /// CHECK: venue init.
    #[account(mut)]
    pub escrow_vault: UncheckedAccount<'info>,
    /// CHECK: venue init.
    #[account(mut)]
    pub bid_vault: UncheckedAccount<'info>,
    /// CHECK: venue's event authority PDA.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, auction_venue::program::AuctionVenue>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_open_swap_rfq(ctx: Context<OpenSwapRfq>, amount_s: u64) -> Result<()> {
    let clock = Clock::get()?;
    let vault = &ctx.accounts.vault;
    require!(
        vault.open_swap_rfqs < vault.config.max_open_rfqs,
        VaultError::TooManyRfqs
    );
    let s_in = amount_s.min(ctx.accounts.proceeds.amount);
    require!(s_in > 0, VaultError::ZeroAmount);

    let (spot, spot_scale) = oracle::spot_cross(
        &ctx.accounts.underlying_price,
        &ctx.accounts.settlement_price,
        &vault.config,
        clock.unix_timestamp as u64,
    )?;
    // Reserve = the band floor on the open-time cross; > 0 guards against
    // dust proceeds that round to nothing (re-checked fresh at settle).
    let reserve = swap_floor(&vault.config, s_in, spot, spot_scale)?;
    require!(reserve > 0, VaultError::ProceedsUnswapped);

    let params = AuctionParams {
        reserve_bid: reserve,
        duration_ms: vault.config.rfq_duration_ms,
        snipe_window_ms: vault.config.rfq_snipe_window_ms,
        snipe_extension_ms: vault.config.rfq_snipe_extension_ms,
        max_extension_ms: vault.config.rfq_max_extension_ms,
        min_increment_bps: vault.config.rfq_min_increment_bps,
        position_recipient: vault.key(),
        settle_authority: Some(vault.key()),
    };
    let salt = vault.auction_nonce;

    vault_seeds!(vault, vsalt, vbump, seeds, signer_seeds);
    auction_venue::cpi::create_swap_auction(
        CpiContext::new_with_signer(
            auction_venue::ID,
            auction_venue::cpi::accounts::CreateAuction {
                payer: ctx.accounts.cranker.to_account_info(),
                creator: ctx.accounts.vault.to_account_info(),
                escrow_mint: ctx.accounts.settlement_mint.to_account_info(),
                bid_mint: ctx.accounts.underlying_mint.to_account_info(),
                auction: ctx.accounts.auction.to_account_info(),
                escrow_vault: ctx.accounts.escrow_vault.to_account_info(),
                bid_vault: ctx.accounts.bid_vault.to_account_info(),
                escrow_source: ctx.accounts.proceeds.to_account_info(),
                // Underlying bids land straight in deployable.
                proceeds_token: ctx.accounts.deployable.to_account_info(),
                // Unfilled/vetoed escrow returns to the proceeds vault.
                refund_token: ctx.accounts.proceeds.to_account_info(),
                bucket: ctx.accounts.vault.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                program: ctx.accounts.venue_program.to_account_info(),
            },
            signer_seeds,
        ),
        salt,
        s_in,
        params,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.auction_nonce += 1;
    vault.open_swap_rfqs += 1;
    emit_cpi!(VaultSwapOpened {
        vault: vault.key(),
        round: vault.round,
        auction: ctx.accounts.auction.key(),
        amount_s: s_in,
        reserve_underlying: reserve,
    });
    Ok(())
}

/// Resolve one of the vault's swap auctions (mirrors
/// `vault::settle_swap_rfq`): the winning bid is re-checked against a
/// FRESH Pyth cross — if it still clears the band, the winner takes the
/// settlement and the vault absorbs the underlying (recording the round's
/// realized rate for the perf fee); if the price moved out of band, or
/// there were no bids, the settlement returns to proceeds for re-auction
/// and the bid is refunded.
#[event_cpi]
#[derive(Accounts)]
pub struct SettleSwapRfq<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(
        mut,
        constraint = auction.creator == vault.key() @ VaultError::WrongOrigin,
        constraint = auction.settle_authority == Some(vault.key()) @ VaultError::WrongOrigin,
    )]
    pub auction: Box<Account<'info, auction_venue::state::Auction>>,
    /// CHECK: venue validates by seeds.
    #[account(mut)]
    pub escrow_vault: UncheckedAccount<'info>,
    #[account(
        mut,
        constraint = bid_vault.key() == Pubkey::find_program_address(
            &[auction_venue::state::BIDS_SEED, auction.key().as_ref()],
            &auction_venue::ID,
        ).0 @ VaultError::AccountMismatch,
    )]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub underlying_price: UncheckedAccount<'info>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub settlement_price: UncheckedAccount<'info>,
    /// CHECK: winner's settlement destination (fill path); venue verifies
    /// ownership.
    #[account(mut)]
    pub winner_dest: Option<UncheckedAccount<'info>>,
    /// CHECK: standing bidder's refund ATA (refund path); venue verifies.
    #[account(mut)]
    pub bidder_refund: Option<UncheckedAccount<'info>>,
    /// CHECK: venue's event authority.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, auction_venue::program::AuctionVenue>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_settle_swap_rfq(ctx: Context<SettleSwapRfq>) -> Result<()> {
    let clock = Clock::get()?;
    let _now = now_ms(&clock);
    let vault = &ctx.accounts.vault;
    let auction = &ctx.accounts.auction;
    let auction_key = auction.key();
    let amount_s = auction.amount;
    let bid = ctx.accounts.bid_vault.amount;
    let bidder = auction.best_bidder;

    // Fresh-cross band check — vault policy, applied before the venue
    // mechanically resolves the auction.
    let (spot, spot_scale) = oracle::spot_cross(
        &ctx.accounts.underlying_price,
        &ctx.accounts.settlement_price,
        &vault.config,
        clock.unix_timestamp as u64,
    )?;
    let u_min = swap_floor(&vault.config, amount_s, spot, spot_scale)?;
    let fill = bidder.is_some() && u_min > 0 && bid >= u_min;

    vault_seeds!(vault, vsalt, vbump, seeds, signer_seeds);
    auction_venue::cpi::settle_swap(
        CpiContext::new_with_signer(
            auction_venue::ID,
            auction_venue::cpi::accounts::SettleSwap {
                cranker: ctx.accounts.cranker.to_account_info(),
                creator_wallet: ctx.accounts.vault.to_account_info(),
                auction: ctx.accounts.auction.to_account_info(),
                escrow_vault: ctx.accounts.escrow_vault.to_account_info(),
                bid_vault: ctx.accounts.bid_vault.to_account_info(),
                authority: Some(ctx.accounts.vault.to_account_info()),
                winner_dest: ctx
                    .accounts
                    .winner_dest
                    .as_ref()
                    .map(|a| a.to_account_info()),
                bidder_refund: ctx
                    .accounts
                    .bidder_refund
                    .as_ref()
                    .map(|a| a.to_account_info()),
                proceeds_token: ctx.accounts.deployable.to_account_info(),
                refund_token: ctx.accounts.proceeds.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                program: ctx.accounts.venue_program.to_account_info(),
            },
            signer_seeds,
        ),
        // Out-of-band standing bid ⇒ the coupled veto refunds it.
        bidder.is_some() && !fill,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.open_swap_rfqs -= 1;
    if fill {
        vault.round_swap_settlement_out += amount_s;
        vault.round_swap_underlying_in += bid;
        emit_cpi!(VaultSwapSettled {
            vault: vault.key(),
            round: vault.round,
            auction: auction_key,
            bidder: bidder.unwrap(),
            settlement_out: amount_s,
            underlying_in: bid,
        });
    } else {
        emit_cpi!(VaultSwapUnfilled {
            vault: vault.key(),
            round: vault.round,
            auction: auction_key,
            amount_s,
        });
    }
    Ok(())
}
