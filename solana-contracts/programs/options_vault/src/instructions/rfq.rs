use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use auction_venue::instructions::create::AuctionParams;

use crate::error::VaultError;
use crate::vault_seeds;
use crate::events::*;
use crate::oracle;
use crate::state::*;
use crate::util::now_ms;

/// Escrow a slice into a coupled call auction (mirrors `vault::open_rfq`).
/// The reserve floor is `min_reserve_premium_bps` of the slice's Pyth
/// spot notional, at least 1 unit — a floor, not a fair price;
/// competition discovers price.
#[event_cpi]
#[derive(Accounts)]
pub struct OpenRfq<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: the venue re-validates the bucket; identity pinned below.
    #[account(constraint = vault.current_bucket == Some(bucket.key()) @ VaultError::BucketNotSelected)]
    pub bucket: UncheckedAccount<'info>,
    #[account(address = vault.underlying_mint)]
    pub underlying_mint: Box<Account<'info, Mint>>,
    #[account(address = vault.settlement_mint)]
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub underlying_price: UncheckedAccount<'info>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub settlement_price: UncheckedAccount<'info>,
    // ── venue CPI accounts (auction PDAs seeded by vault + nonce) ──
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

pub fn handle_open_rfq(ctx: Context<OpenRfq>, slice_amount: u64) -> Result<()> {
    let clock = Clock::get()?;
    let now = now_ms(&clock);
    let vault = &ctx.accounts.vault;

    require!(vault.phase == Phase::Active, VaultError::WrongPhase);
    require!(now < vault.selling_ends_ms, VaultError::SellingClosed);
    require!(
        vault.open_rfqs < vault.config.max_open_rfqs,
        VaultError::TooManyRfqs
    );
    require!(slice_amount > 0, VaultError::ZeroAmount);
    require!(
        slice_amount <= vault.config.max_slice_amount
            && slice_amount <= ctx.accounts.deployable.amount,
        VaultError::SliceTooLarge
    );

    let (spot, spot_scale) = oracle::spot_cross(
        &ctx.accounts.underlying_price,
        &ctx.accounts.settlement_price,
        &vault.config,
        clock.unix_timestamp as u64,
    )?;
    let notional = options_math::settlement_notional(slice_amount, spot, spot_scale)
        .ok_or(VaultError::MathOverflow)?;
    let reserve = (((notional as u128) * (vault.config.min_reserve_premium_bps as u128)
        / options_math::BPS_DENOM) as u64)
        .max(1);

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
    auction_venue::cpi::create_call_auction(
        CpiContext::new_with_signer(
            auction_venue::ID,
            auction_venue::cpi::accounts::CreateAuction {
                payer: ctx.accounts.cranker.to_account_info(),
                creator: ctx.accounts.vault.to_account_info(),
                escrow_mint: ctx.accounts.underlying_mint.to_account_info(),
                bid_mint: ctx.accounts.settlement_mint.to_account_info(),
                auction: ctx.accounts.auction.to_account_info(),
                escrow_vault: ctx.accounts.escrow_vault.to_account_info(),
                bid_vault: ctx.accounts.bid_vault.to_account_info(),
                escrow_source: ctx.accounts.deployable.to_account_info(),
                proceeds_token: ctx.accounts.proceeds.to_account_info(),
                refund_token: ctx.accounts.deployable.to_account_info(),
                bucket: ctx.accounts.bucket.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
                event_authority: ctx.accounts.venue_event_authority.to_account_info(),
                program: ctx.accounts.venue_program.to_account_info(),
            },
            signer_seeds,
        ),
        salt,
        slice_amount,
        params,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.auction_nonce += 1;
    vault.open_rfqs += 1;
    emit_cpi!(VaultRfqOpened {
        vault: vault.key(),
        round: vault.round,
        auction: ctx.accounts.auction.key(),
        slice_amount,
        reserve_premium: reserve,
    });
    Ok(())
}

/// Resolve one of the vault's coupled call auctions (mirrors
/// `vault::settle_rfq`): the winner gets the call coins, the vault
/// absorbs the Position (owner = vault PDA) and the net premium into its
/// proceeds vault, and the FIFO gains an index entry. No winner: the
/// escrow returns to deployable via the venue's refund path.
#[event_cpi]
#[derive(Accounts)]
pub struct SettleRfq<'info> {
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
    /// CHECK: venue-owned; venue validates.
    #[account(mut)]
    pub escrow_vault: UncheckedAccount<'info>,
    /// CHECK: venue-owned bid vault; venue validates by seeds.
    #[account(mut)]
    pub bid_vault: UncheckedAccount<'info>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    /// The FIFO tail entry, created only when a winner exists.
    #[account(
        init,
        payer = cranker,
        space = 8 + VaultPosition::INIT_SPACE,
        seeds = [VAULT_POS_SEED, vault.key().as_ref(), &vault.positions_tail.to_le_bytes()],
        bump
    )]
    pub vault_position: Box<Account<'info, VaultPosition>>,
    // ── pass-through to venue settle_call ──
    /// CHECK: bucket, core re-validates.
    #[account(mut)]
    pub bucket: UncheckedAccount<'info>,
    /// CHECK: fresh keypair (core position init).
    #[account(mut)]
    pub position: Signer<'info>,
    /// CHECK: core enforces.
    #[account(mut)]
    pub bucket_underlying_vault: UncheckedAccount<'info>,
    /// CHECK: core enforces.
    #[account(mut)]
    pub call_mint: UncheckedAccount<'info>,
    /// CHECK: venue enforces winner-recipient ownership.
    #[account(mut)]
    pub call_dest: UncheckedAccount<'info>,
    /// CHECK: venue reads fee_bps.
    pub core_config: UncheckedAccount<'info>,
    /// CHECK: venue verifies treasury ownership.
    #[account(mut)]
    pub core_treasury_token: UncheckedAccount<'info>,
    /// CHECK: core's event authority.
    pub core_event_authority: UncheckedAccount<'info>,
    pub core_program: Program<'info, options_core::program::OptionsCore>,
    /// CHECK: venue's event authority.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, auction_venue::program::AuctionVenue>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_settle_rfq(ctx: Context<SettleRfq>) -> Result<()> {
    let auction = &ctx.accounts.auction;
    let had_winner = auction.best_bidder.is_some();
    let auction_key = auction.key();
    let amount = auction.amount;
    let proceeds_before = ctx.accounts.proceeds.amount;

    let vault = &ctx.accounts.vault;
    vault_seeds!(vault, vsalt, vbump, seeds, signer_seeds);
    auction_venue::cpi::settle_call(CpiContext::new_with_signer(
        auction_venue::ID,
        auction_venue::cpi::accounts::SettleCall {
            cranker: ctx.accounts.cranker.to_account_info(),
            creator_wallet: ctx.accounts.vault.to_account_info(),
            auction: ctx.accounts.auction.to_account_info(),
            escrow_vault: ctx.accounts.escrow_vault.to_account_info(),
            bid_vault: ctx.accounts.bid_vault.to_account_info(),
            authority: Some(ctx.accounts.vault.to_account_info()),
            proceeds_token: ctx.accounts.proceeds.to_account_info(),
            refund_token: ctx.accounts.deployable.to_account_info(),
            bucket: ctx.accounts.bucket.to_account_info(),
            position: ctx.accounts.position.to_account_info(),
            underlying_vault: ctx.accounts.bucket_underlying_vault.to_account_info(),
            call_mint: ctx.accounts.call_mint.to_account_info(),
            call_dest: ctx.accounts.call_dest.to_account_info(),
            core_config: ctx.accounts.core_config.to_account_info(),
            core_treasury_token: ctx.accounts.core_treasury_token.to_account_info(),
            core_event_authority_acc: ctx.accounts.core_event_authority.to_account_info(),
            core_program: ctx.accounts.core_program.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
            event_authority: ctx.accounts.venue_event_authority.to_account_info(),
            program: ctx.accounts.venue_program.to_account_info(),
        },
        signer_seeds,
    ))?;

    if had_winner {
        // Verify the CPI produced the vault's position on the right
        // bucket, then index it at the FIFO tail.
        let data = ctx.accounts.position.try_borrow_data()?;
        let pos = options_core::state::Position::try_deserialize(&mut &data[..])?;
        drop(data);
        require!(
            pos.owner == ctx.accounts.vault.key()
                && Some(pos.bucket) == ctx.accounts.vault.current_bucket,
            VaultError::AccountMismatch
        );

        ctx.accounts.proceeds.reload()?;
        let net = ctx.accounts.proceeds.amount - proceeds_before;

        let tail = ctx.accounts.vault.positions_tail;
        let vp = &mut ctx.accounts.vault_position;
        vp.vault = ctx.accounts.vault.key();
        vp.index = tail;
        vp.position = ctx.accounts.position.key();
        vp.bump = ctx.bumps.vault_position;

        let vault = &mut ctx.accounts.vault;
        vault.positions_tail += 1;
        vault.round_premium_collected += net;
        vault.open_rfqs -= 1;
        emit_cpi!(VaultRfqSettled {
            vault: vault.key(),
            round: vault.round,
            auction: auction_key,
            position: ctx.accounts.position.key(),
            amount,
            net_premium: net,
        });
    } else {
        // No winner: the venue refunded the escrow into deployable. The
        // just-created FIFO entry is unused — close it back to the
        // cranker so no index slot is burned.
        let vp_info = ctx.accounts.vault_position.to_account_info();
        let cranker_info = ctx.accounts.cranker.to_account_info();
        {
            // Wipe the discriminator so the account can't be revived,
            // then drain lamports — it is purged at transaction end.
            let mut data = vp_info.try_borrow_mut_data()?;
            data[..8].fill(0);
        }
        let lamports = vp_info.lamports();
        **vp_info.try_borrow_mut_lamports()? = 0;
        **cranker_info.try_borrow_mut_lamports()? += lamports;
        vp_info.assign(&anchor_lang::system_program::ID);

        let vault = &mut ctx.accounts.vault;
        vault.open_rfqs -= 1;
        emit_cpi!(VaultRfqUnsold {
            vault: vault.key(),
            round: vault.round,
            auction: auction_key,
            amount,
        });
    }
    Ok(())
}

/// Recovery twin for coupled auctions on a dead bucket (mirrors
/// `vault::settle_rfq_expired`): refund the bid to the bidder, absorb the
/// collateral back into deployable. Permissionless, so no admin is ever
/// needed to unstick a round.
#[event_cpi]
#[derive(Accounts)]
pub struct SettleRfqExpired<'info> {
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
    /// CHECK: venue validates.
    #[account(mut)]
    pub escrow_vault: UncheckedAccount<'info>,
    /// CHECK: venue validates.
    #[account(mut)]
    pub bid_vault: UncheckedAccount<'info>,
    /// CHECK: bucket, deserialized by the venue.
    pub bucket: UncheckedAccount<'info>,
    /// CHECK: outbid bidder's refund ATA; venue verifies.
    #[account(mut)]
    pub bidder_refund: Option<UncheckedAccount<'info>>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    /// CHECK: venue's event authority.
    pub venue_event_authority: UncheckedAccount<'info>,
    pub venue_program: Program<'info, auction_venue::program::AuctionVenue>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_settle_rfq_expired(ctx: Context<SettleRfqExpired>) -> Result<()> {
    let auction_key = ctx.accounts.auction.key();
    let amount = ctx.accounts.auction.amount;
    let vault = &ctx.accounts.vault;
    vault_seeds!(vault, vsalt, vbump, seeds, signer_seeds);
    auction_venue::cpi::settle_expired(CpiContext::new_with_signer(
        auction_venue::ID,
        auction_venue::cpi::accounts::SettleExpired {
            cranker: ctx.accounts.cranker.to_account_info(),
            creator_wallet: ctx.accounts.vault.to_account_info(),
            auction: ctx.accounts.auction.to_account_info(),
            escrow_vault: ctx.accounts.escrow_vault.to_account_info(),
            bid_vault: ctx.accounts.bid_vault.to_account_info(),
            authority: Some(ctx.accounts.vault.to_account_info()),
            bucket: ctx.accounts.bucket.to_account_info(),
            bidder_refund: ctx
                .accounts
                .bidder_refund
                .as_ref()
                .map(|a| a.to_account_info()),
            refund_token: ctx.accounts.deployable.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            event_authority: ctx.accounts.venue_event_authority.to_account_info(),
            program: ctx.accounts.venue_program.to_account_info(),
        },
        signer_seeds,
    ))?;
    let vault = &mut ctx.accounts.vault;
    vault.open_rfqs -= 1;
    emit_cpi!(VaultRfqUnsold {
        vault: vault.key(),
        round: vault.round,
        auction: auction_key,
        amount,
    });
    Ok(())
}
