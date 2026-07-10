use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount};

use crate::error::VenueError;
use crate::events::*;
use crate::instructions::create::deserialize_bucket;
use crate::state::*;
use crate::util::now_ms;

/// Core's treasury PDA — where the venue routes the protocol fee. A plain
/// SPL transfer into the treasury's ATA is equivalent to
/// `deposit_protocol_fee` minus the event (the venue emits its own
/// `AuctionSettled` with the fee field for indexing).
pub fn core_treasury_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"treasury"], &options_core::ID).0
}

pub fn core_event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &options_core::ID).0
}

fn assert_settle_authority(
    auction: &Auction,
    authority: &Option<Signer>,
) -> Result<()> {
    if let Some(expected) = auction.settle_authority {
        let signer = authority
            .as_ref()
            .ok_or(VenueError::WrongSettleAuthority)?;
        require!(
            signer.key() == expected,
            VenueError::WrongSettleAuthority
        );
    }
    Ok(())
}

macro_rules! auction_seeds {
    ($auction:expr, $salt:ident, $bump:ident, $seeds:ident, $signer:ident) => {
        let $salt = $auction.salt.to_le_bytes();
        let $bump = [$auction.bump];
        let creator = $auction.creator;
        let $seeds: [&[u8]; 4] = [AUCTION_SEED, creator.as_ref(), &$salt, &$bump];
        let $signer: &[&[&[u8]]] = &[&$seeds];
    };
}

/// Settle a closed covered-call auction (mirrors `rfq::settle` /
/// `rfq::finalize`). Winner: the covered write executes via CPI into
/// options_core — the winner's recipient gets the call coins, the fixed
/// `position_recipient` owns the Position, the protocol fee goes to
/// core's treasury, and the net premium lands in the `proceeds_token`
/// fixed at creation. No winner: the escrow returns to `refund_token`.
/// Coupled auctions additionally require the settle authority's
/// signature (the vault PDA, signing via CPI).
#[event_cpi]
#[derive(Accounts)]
pub struct SettleCall<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    /// CHECK: rent destination for the closed auction accounts.
    #[account(mut, address = auction.creator)]
    pub creator_wallet: UncheckedAccount<'info>,
    #[account(
        mut,
        close = creator_wallet,
        constraint = auction.mode == AuctionMode::CoveredCall @ VenueError::WrongMode,
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(mut, seeds = [ESCROW_SEED, auction.key().as_ref()], bump)]
    pub escrow_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [BIDS_SEED, auction.key().as_ref()], bump)]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    pub authority: Option<Signer<'info>>,
    #[account(mut, address = auction.proceeds_token @ VenueError::RecipientMismatch)]
    pub proceeds_token: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = auction.refund_token @ VenueError::RecipientMismatch)]
    pub refund_token: Box<Account<'info, TokenAccount>>,
    // ── options_core CPI accounts ──
    /// CHECK: validated against auction.bucket; core re-validates fully.
    #[account(mut, address = auction.bucket @ VenueError::BucketMismatch)]
    pub bucket: UncheckedAccount<'info>,
    /// CHECK: fresh keypair for the Position (core init).
    #[account(mut)]
    pub position: Signer<'info>,
    /// CHECK: the bucket's underlying vault; core enforces its identity.
    #[account(mut)]
    pub underlying_vault: UncheckedAccount<'info>,
    /// CHECK: the bucket's call mint; core enforces address.
    #[account(mut)]
    pub call_mint: UncheckedAccount<'info>,
    /// Winner's call-coin destination; owner pinned to the winning bid's
    /// token_recipient in the handler, mint enforced by core.
    #[account(mut)]
    pub call_dest: Box<Account<'info, TokenAccount>>,
    pub core_config: Box<Account<'info, options_core::state::Config>>,
    /// Core treasury's ATA for the bid mint (fee destination).
    #[account(mut, token::mint = auction.bid_mint)]
    pub core_treasury_token: Box<Account<'info, TokenAccount>>,
    /// CHECK: core's event authority PDA.
    pub core_event_authority_acc: UncheckedAccount<'info>,
    pub core_program: Program<'info, options_core::program::OptionsCore>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_settle_call(ctx: Context<SettleCall>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let auction = &ctx.accounts.auction;
    require!(now >= auction.deadline_ms, VenueError::AuctionNotClosed);
    assert_settle_authority(auction, &ctx.accounts.authority)?;

    auction_seeds!(auction, salt, bump, seeds, signer_seeds);
    let gross_bid = ctx.accounts.bid_vault.amount;

    if let Some(winner) = auction.best_bidder {
        let token_recipient = auction.best_token_recipient.unwrap();
        require!(
            ctx.accounts.call_dest.owner == token_recipient,
            VenueError::RecipientMismatch
        );
        require!(
            ctx.accounts.core_treasury_token.owner == core_treasury_pda(),
            VenueError::RecipientMismatch
        );

        // Execute the covered write: escrow → bucket, Position →
        // position_recipient, calls → winner's recipient.
        options_core::cpi::write_collateralized(
            CpiContext::new_with_signer(
                options_core::ID,
                options_core::cpi::accounts::WriteCollateralized {
                    payer: ctx.accounts.cranker.to_account_info(),
                    writer: ctx.accounts.auction.to_account_info(),
                    bucket: ctx.accounts.bucket.to_account_info(),
                    position: ctx.accounts.position.to_account_info(),
                    writer_underlying: ctx.accounts.escrow_vault.to_account_info(),
                    underlying_vault: ctx.accounts.underlying_vault.to_account_info(),
                    call_mint: ctx.accounts.call_mint.to_account_info(),
                    call_dest: ctx.accounts.call_dest.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                    event_authority: ctx.accounts.core_event_authority_acc.to_account_info(),
                    program: ctx.accounts.core_program.to_account_info(),
                },
                signer_seeds,
            ),
            auction.amount,
            auction.position_recipient,
        )?;

        // Fee skim to core's treasury, net premium to the proceeds
        // account fixed at creation.
        let fee = options_math::fee_amount(gross_bid, ctx.accounts.core_config.fee_bps);
        let net = gross_bid - fee;
        if fee > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.bid_vault.to_account_info(),
                        to: ctx.accounts.core_treasury_token.to_account_info(),
                        authority: ctx.accounts.auction.to_account_info(),
                    },
                    signer_seeds,
                ),
                fee,
            )?;
        }
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.bid_vault.to_account_info(),
                    to: ctx.accounts.proceeds_token.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            net,
        )?;

        emit_cpi!(AuctionSettled {
            auction: auction.key(),
            mode: auction.mode,
            bucket: auction.bucket,
            winner,
            token_recipient,
            position: ctx.accounts.position.key(),
            position_recipient: auction.position_recipient,
            amount: auction.amount,
            notional: auction.notional,
            gross_bid,
            fee,
            net_proceeds: net,
        });
    } else {
        // No bids: refund the escrow.
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.escrow_vault.to_account_info(),
                    to: ctx.accounts.refund_token.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            auction.amount,
        )?;
        emit_cpi!(AuctionUnsold {
            auction: auction.key(),
            mode: auction.mode,
            bucket: auction.bucket,
            amount: auction.amount,
            reserve_bid: auction.reserve_bid,
            bid_refunded: false,
        });
    }

    close_vaults(
        &ctx.accounts.escrow_vault,
        &ctx.accounts.bid_vault,
        &ctx.accounts.auction,
        &ctx.accounts.creator_wallet,
        signer_seeds,
    )
}

/// Settle a closed cash-secured-put auction (mirrors `rfq_put::settle`).
#[event_cpi]
#[derive(Accounts)]
pub struct SettlePut<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    /// CHECK: rent destination for the closed auction accounts.
    #[account(mut, address = auction.creator)]
    pub creator_wallet: UncheckedAccount<'info>,
    #[account(
        mut,
        close = creator_wallet,
        constraint = auction.mode == AuctionMode::CashSecuredPut @ VenueError::WrongMode,
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(mut, seeds = [ESCROW_SEED, auction.key().as_ref()], bump)]
    pub escrow_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [BIDS_SEED, auction.key().as_ref()], bump)]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    pub authority: Option<Signer<'info>>,
    #[account(mut, address = auction.proceeds_token @ VenueError::RecipientMismatch)]
    pub proceeds_token: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = auction.refund_token @ VenueError::RecipientMismatch)]
    pub refund_token: Box<Account<'info, TokenAccount>>,
    // ── options_core CPI accounts ──
    /// CHECK: validated against auction.bucket; core re-validates fully.
    #[account(mut, address = auction.bucket @ VenueError::BucketMismatch)]
    pub bucket: UncheckedAccount<'info>,
    /// CHECK: fresh keypair for the Position (core init).
    #[account(mut)]
    pub position: Signer<'info>,
    /// CHECK: the bucket's settlement vault; core enforces its identity.
    #[account(mut)]
    pub settlement_vault: UncheckedAccount<'info>,
    /// CHECK: the bucket's put mint; core enforces address.
    #[account(mut)]
    pub put_mint: UncheckedAccount<'info>,
    /// Winner's put-coin destination.
    #[account(mut)]
    pub put_dest: Box<Account<'info, TokenAccount>>,
    pub core_config: Box<Account<'info, options_core::state::Config>>,
    #[account(mut, token::mint = auction.bid_mint)]
    pub core_treasury_token: Box<Account<'info, TokenAccount>>,
    /// CHECK: core's event authority PDA.
    pub core_event_authority_acc: UncheckedAccount<'info>,
    pub core_program: Program<'info, options_core::program::OptionsCore>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_settle_put(ctx: Context<SettlePut>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let auction = &ctx.accounts.auction;
    require!(now >= auction.deadline_ms, VenueError::AuctionNotClosed);
    assert_settle_authority(auction, &ctx.accounts.authority)?;

    auction_seeds!(auction, salt, bump, seeds, signer_seeds);
    let gross_bid = ctx.accounts.bid_vault.amount;

    if let Some(winner) = auction.best_bidder {
        let token_recipient = auction.best_token_recipient.unwrap();
        require!(
            ctx.accounts.put_dest.owner == token_recipient,
            VenueError::RecipientMismatch
        );
        require!(
            ctx.accounts.core_treasury_token.owner == core_treasury_pda(),
            VenueError::RecipientMismatch
        );

        options_core::cpi::write_put_collateralized(
            CpiContext::new_with_signer(
                options_core::ID,
                options_core::cpi::accounts::WritePutCollateralized {
                    payer: ctx.accounts.cranker.to_account_info(),
                    writer: ctx.accounts.auction.to_account_info(),
                    bucket: ctx.accounts.bucket.to_account_info(),
                    position: ctx.accounts.position.to_account_info(),
                    writer_settlement: ctx.accounts.escrow_vault.to_account_info(),
                    settlement_vault: ctx.accounts.settlement_vault.to_account_info(),
                    put_mint: ctx.accounts.put_mint.to_account_info(),
                    put_dest: ctx.accounts.put_dest.to_account_info(),
                    token_program: ctx.accounts.token_program.to_account_info(),
                    system_program: ctx.accounts.system_program.to_account_info(),
                    event_authority: ctx.accounts.core_event_authority_acc.to_account_info(),
                    program: ctx.accounts.core_program.to_account_info(),
                },
                signer_seeds,
            ),
            auction.notional,
            auction.position_recipient,
        )?;

        let fee = options_math::fee_amount(gross_bid, ctx.accounts.core_config.fee_bps);
        let net = gross_bid - fee;
        if fee > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.bid_vault.to_account_info(),
                        to: ctx.accounts.core_treasury_token.to_account_info(),
                        authority: ctx.accounts.auction.to_account_info(),
                    },
                    signer_seeds,
                ),
                fee,
            )?;
        }
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.bid_vault.to_account_info(),
                    to: ctx.accounts.proceeds_token.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            net,
        )?;

        emit_cpi!(AuctionSettled {
            auction: auction.key(),
            mode: auction.mode,
            bucket: auction.bucket,
            winner,
            token_recipient,
            position: ctx.accounts.position.key(),
            position_recipient: auction.position_recipient,
            amount: auction.amount,
            notional: auction.notional,
            gross_bid,
            fee,
            net_proceeds: net,
        });
    } else {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.escrow_vault.to_account_info(),
                    to: ctx.accounts.refund_token.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            auction.amount,
        )?;
        emit_cpi!(AuctionUnsold {
            auction: auction.key(),
            mode: auction.mode,
            bucket: auction.bucket,
            amount: auction.amount,
            reserve_bid: auction.reserve_bid,
            bid_refunded: false,
        });
    }

    close_vaults(
        &ctx.accounts.escrow_vault,
        &ctx.accounts.bid_vault,
        &ctx.accounts.auction,
        &ctx.accounts.creator_wallet,
        signer_seeds,
    )
}

/// Settle a closed pure-swap auction. Winner (unless `force_refund`):
/// escrow → winner, bid → proceeds. `force_refund` is the coupled
/// venue's out-of-band veto (the vault's fresh-Pyth band check lives in
/// the VAULT, not here — the venue stays price-agnostic): bid → bidder,
/// escrow → refund.
#[event_cpi]
#[derive(Accounts)]
pub struct SettleSwap<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    /// CHECK: rent destination for the closed auction accounts.
    #[account(mut, address = auction.creator)]
    pub creator_wallet: UncheckedAccount<'info>,
    #[account(
        mut,
        close = creator_wallet,
        constraint = auction.mode == AuctionMode::Swap @ VenueError::WrongMode,
    )]
    pub auction: Box<Account<'info, Auction>>,
    #[account(mut, seeds = [ESCROW_SEED, auction.key().as_ref()], bump)]
    pub escrow_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [BIDS_SEED, auction.key().as_ref()], bump)]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    pub authority: Option<Signer<'info>>,
    /// Winner's escrow destination (fill path).
    #[account(mut, token::mint = auction.escrow_mint)]
    pub winner_dest: Option<Box<Account<'info, TokenAccount>>>,
    /// Standing bidder's refund ATA (force_refund path).
    #[account(mut)]
    pub bidder_refund: Option<Box<Account<'info, TokenAccount>>>,
    #[account(mut, address = auction.proceeds_token @ VenueError::RecipientMismatch)]
    pub proceeds_token: Box<Account<'info, TokenAccount>>,
    #[account(mut, address = auction.refund_token @ VenueError::RecipientMismatch)]
    pub refund_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_settle_swap(ctx: Context<SettleSwap>, force_refund: bool) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let auction = &ctx.accounts.auction;
    require!(now >= auction.deadline_ms, VenueError::AuctionNotClosed);
    assert_settle_authority(auction, &ctx.accounts.authority)?;
    if force_refund {
        // Only a coupled venue may veto a fill (its oracle-band policy).
        require!(
            auction.settle_authority.is_some(),
            VenueError::ForceRefundUnauthorized
        );
    }

    auction_seeds!(auction, salt, bump, seeds, signer_seeds);
    let gross_bid = ctx.accounts.bid_vault.amount;

    match (auction.best_bidder, force_refund) {
        (Some(winner), false) => {
            let winner_dest = ctx
                .accounts
                .winner_dest
                .as_ref()
                .ok_or(VenueError::RecipientMismatch)?;
            require!(winner_dest.owner == winner, VenueError::RecipientMismatch);
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.escrow_vault.to_account_info(),
                        to: winner_dest.to_account_info(),
                        authority: ctx.accounts.auction.to_account_info(),
                    },
                    signer_seeds,
                ),
                auction.amount,
            )?;
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.bid_vault.to_account_info(),
                        to: ctx.accounts.proceeds_token.to_account_info(),
                        authority: ctx.accounts.auction.to_account_info(),
                    },
                    signer_seeds,
                ),
                gross_bid,
            )?;
            emit_cpi!(AuctionSettled {
                auction: auction.key(),
                mode: auction.mode,
                bucket: auction.bucket,
                winner,
                token_recipient: winner_dest.key(),
                position: Pubkey::default(),
                position_recipient: Pubkey::default(),
                amount: auction.amount,
                notional: 0,
                gross_bid,
                fee: 0,
                net_proceeds: gross_bid,
            });
        }
        (best_bidder, _) => {
            // No bids, or the coupled venue vetoed the fill: escrow back,
            // standing bid (if any) refunded to the bidder's ATA.
            if let Some(bidder) = best_bidder {
                let refund = ctx
                    .accounts
                    .bidder_refund
                    .as_ref()
                    .ok_or(VenueError::RefundAccountMismatch)?;
                let expected = anchor_spl::associated_token::get_associated_token_address(
                    &bidder,
                    &auction.bid_mint,
                );
                require!(refund.key() == expected, VenueError::RefundAccountMismatch);
                token::transfer(
                    CpiContext::new_with_signer(
                        token::ID,
                        token::Transfer {
                            from: ctx.accounts.bid_vault.to_account_info(),
                            to: refund.to_account_info(),
                            authority: ctx.accounts.auction.to_account_info(),
                        },
                        signer_seeds,
                    ),
                    gross_bid,
                )?;
            }
            token::transfer(
                CpiContext::new_with_signer(
                    token::ID,
                    token::Transfer {
                        from: ctx.accounts.escrow_vault.to_account_info(),
                        to: ctx.accounts.refund_token.to_account_info(),
                        authority: ctx.accounts.auction.to_account_info(),
                    },
                    signer_seeds,
                ),
                auction.amount,
            )?;
            emit_cpi!(AuctionUnsold {
                auction: auction.key(),
                mode: auction.mode,
                bucket: auction.bucket,
                amount: auction.amount,
                reserve_bid: auction.reserve_bid,
                bid_refunded: best_bidder.is_some(),
            });
        }
    }

    close_vaults(
        &ctx.accounts.escrow_vault,
        &ctx.accounts.bid_vault,
        &ctx.accounts.auction,
        &ctx.accounts.creator_wallet,
        signer_seeds,
    )
}

/// Recovery path (mirrors `rfq::settle_expired`): the bucket died
/// mid-auction (expired or invalidated) so the write can never execute —
/// refund both escrows so funds can never strand. No deadline
/// precondition: once the bucket is dead the auction is moot.
#[event_cpi]
#[derive(Accounts)]
pub struct SettleExpired<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    /// CHECK: rent destination for the closed auction accounts.
    #[account(mut, address = auction.creator)]
    pub creator_wallet: UncheckedAccount<'info>,
    #[account(mut, close = creator_wallet)]
    pub auction: Box<Account<'info, Auction>>,
    #[account(mut, seeds = [ESCROW_SEED, auction.key().as_ref()], bump)]
    pub escrow_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [BIDS_SEED, auction.key().as_ref()], bump)]
    pub bid_vault: Box<Account<'info, TokenAccount>>,
    pub authority: Option<Signer<'info>>,
    /// CHECK: validated against auction.bucket; deserialized per-mode.
    #[account(address = auction.bucket @ VenueError::BucketMismatch)]
    pub bucket: UncheckedAccount<'info>,
    #[account(mut)]
    pub bidder_refund: Option<Box<Account<'info, TokenAccount>>>,
    #[account(mut, address = auction.refund_token @ VenueError::RecipientMismatch)]
    pub refund_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_settle_expired(ctx: Context<SettleExpired>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let auction = &ctx.accounts.auction;
    assert_settle_authority(auction, &ctx.accounts.authority)?;

    // The bucket must actually be dead.
    let (expiry_ms, invalidated) = match auction.mode {
        AuctionMode::CoveredCall => {
            let b = deserialize_bucket::<options_core::state::Bucket>(&ctx.accounts.bucket)?;
            (b.expiry_ms, b.invalidated)
        }
        AuctionMode::CashSecuredPut => {
            let b = deserialize_bucket::<options_core::state::PutBucket>(&ctx.accounts.bucket)?;
            (b.expiry_ms, b.invalidated)
        }
        AuctionMode::Swap => return err!(VenueError::WrongMode),
    };
    require!(now >= expiry_ms || invalidated, VenueError::BucketStillLive);

    auction_seeds!(auction, salt, bump, seeds, signer_seeds);
    let gross_bid = ctx.accounts.bid_vault.amount;

    if let Some(bidder) = auction.best_bidder {
        let refund = ctx
            .accounts
            .bidder_refund
            .as_ref()
            .ok_or(VenueError::RefundAccountMismatch)?;
        let expected = anchor_spl::associated_token::get_associated_token_address(
            &bidder,
            &auction.bid_mint,
        );
        require!(refund.key() == expected, VenueError::RefundAccountMismatch);
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.bid_vault.to_account_info(),
                    to: refund.to_account_info(),
                    authority: ctx.accounts.auction.to_account_info(),
                },
                signer_seeds,
            ),
            gross_bid,
        )?;
    }
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.escrow_vault.to_account_info(),
                to: ctx.accounts.refund_token.to_account_info(),
                authority: ctx.accounts.auction.to_account_info(),
            },
            signer_seeds,
        ),
        auction.amount,
    )?;
    emit_cpi!(AuctionUnsold {
        auction: auction.key(),
        mode: auction.mode,
        bucket: auction.bucket,
        amount: auction.amount,
        reserve_bid: auction.reserve_bid,
        bid_refunded: auction.best_bidder.is_some(),
    });

    close_vaults(
        &ctx.accounts.escrow_vault,
        &ctx.accounts.bid_vault,
        &ctx.accounts.auction,
        &ctx.accounts.creator_wallet,
        signer_seeds,
    )
}

/// Close both token vaults, rent to the creator (the auction account
/// itself closes via the `close =` attribute).
fn close_vaults<'info>(
    escrow_vault: &Account<'info, TokenAccount>,
    bid_vault: &Account<'info, TokenAccount>,
    auction: &Account<'info, Auction>,
    creator_wallet: &UncheckedAccount<'info>,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    token::close_account(CpiContext::new_with_signer(
        token::ID,
        token::CloseAccount {
            account: escrow_vault.to_account_info(),
            destination: creator_wallet.to_account_info(),
            authority: auction.to_account_info(),
        },
        signer_seeds,
    ))?;
    token::close_account(CpiContext::new_with_signer(
        token::ID,
        token::CloseAccount {
            account: bid_vault.to_account_info(),
            destination: creator_wallet.to_account_info(),
            authority: auction.to_account_info(),
        },
        signer_seeds,
    ))
}
