use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount};

use crate::error::VenueError;
use crate::events::*;
use crate::state::*;
use crate::util::now_ms;

/// Escrow a bid (mirrors `rfq::bid` / `swap_auction::bid`). Must beat
/// `max(reserve, best × (1 + increment))` and strictly beat the standing
/// best. The outbid party is refunded by push transfer to their ATA —
/// on Solana the previous bidder's refund account rides along in the new
/// bidder's transaction (Sui pushed to an address natively). A previous
/// bidder who closes their ATA only griefs themselves: nobody can outbid
/// them, so they win at their own price above the reserve.
#[event_cpi]
#[derive(Accounts)]
pub struct Bid<'info> {
    #[account(mut)]
    pub bidder: Signer<'info>,
    #[account(mut)]
    pub auction: Box<Account<'info, Auction>>,
    #[account(
        mut,
        seeds = [BIDS_SEED, auction.key().as_ref()],
        bump,
    )]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = auction.bid_mint)]
    pub bidder_source: Box<Account<'info, TokenAccount>>,
    /// The outbid bidder's ATA for the bid mint; required when a best bid
    /// stands.
    #[account(mut)]
    pub previous_bidder_refund: Option<Box<Account<'info, TokenAccount>>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_bid(ctx: Context<Bid>, amount: u64, token_recipient: Pubkey) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let auction = &ctx.accounts.auction;
    require!(now < auction.deadline_ms, VenueError::AuctionClosed);

    let previous = ctx.accounts.bid_vault.amount;
    let floor = if auction.best_bidder.is_some() {
        // Ceiling division forces a real improvement; the strict `>`
        // handles min_increment_bps == 0.
        require!(amount > previous, VenueError::BidTooLow);
        options_math::min_next_bid(previous, auction.min_increment_bps)
            .ok_or(VenueError::MathOverflow)?
            .max(auction.reserve_bid)
    } else {
        auction.reserve_bid
    };
    require!(amount >= floor, VenueError::BidTooLow);

    let auction_seeds_salt = auction.salt.to_le_bytes();
    let auction_bump = [auction.bump];
    let creator = auction.creator;
    let signer_seeds_arr: [&[u8]; 4] = [
        AUCTION_SEED,
        creator.as_ref(),
        &auction_seeds_salt,
        &auction_bump,
    ];
    let signer_seeds: &[&[&[u8]]] = &[&signer_seeds_arr];

    // Refund the outbid party first.
    if let Some(prev_bidder) = auction.best_bidder {
        let refund_account = ctx
            .accounts
            .previous_bidder_refund
            .as_ref()
            .ok_or(VenueError::RefundAccountMismatch)?;
        let expected = anchor_spl::associated_token::get_associated_token_address(
            &prev_bidder,
            &auction.bid_mint,
        );
        require!(
            refund_account.key() == expected,
            VenueError::RefundAccountMismatch
        );
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.bid_vault.to_account_info(),
                    to: refund_account.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            previous,
        )?;
    }

    // Escrow the new bid.
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.bidder_source.to_account_info(),
                to: ctx.accounts.bid_vault.to_account_info(),
                authority: ctx.accounts.bidder.to_account_info(),
            },
        ),
        amount,
    )?;

    let auction = &mut ctx.accounts.auction;
    auction.best_bidder = Some(ctx.accounts.bidder.key());
    auction.best_token_recipient = Some(token_recipient);

    // Anti-snipe: late best bids extend the deadline (capped), turning a
    // last-block snipe into an open price war.
    if auction.deadline_ms - now < auction.snipe_window_ms {
        let extended = now + auction.snipe_extension_ms;
        auction.deadline_ms = extended.min(auction.max_deadline_ms);
    }

    emit_cpi!(AuctionBid {
        auction: auction.key(),
        bidder: ctx.accounts.bidder.key(),
        token_recipient,
        bid: amount,
        previous_bid: previous,
        deadline_ms: auction.deadline_ms,
    });
    Ok(())
}
