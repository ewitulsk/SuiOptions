use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::error::VaultError;
use crate::events::*;
use crate::state::*;

#[event_cpi]
#[derive(Accounts)]
#[instruction(salt: u64)]
pub struct CreateVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(constraint = underlying_mint.key() != settlement_mint.key() @ VaultError::AccountMismatch)]
    pub underlying_mint: Box<Account<'info, Mint>>,
    pub settlement_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = admin,
        space = 8 + Vault::INIT_SPACE,
        seeds = [
            VAULT_SEED,
            underlying_mint.key().as_ref(),
            settlement_mint.key().as_ref(),
            &salt.to_le_bytes(),
        ],
        bump
    )]
    pub vault: Box<Account<'info, Vault>>,
    /// The share coin: Sui's fresh `TreasuryCap<VShare>` becomes a mint
    /// created here with the vault PDA as sole authority — zero supply by
    /// construction.
    #[account(
        init,
        payer = admin,
        seeds = [SHARE_MINT_SEED, vault.key().as_ref()],
        bump,
        mint::decimals = underlying_mint.decimals,
        mint::authority = vault,
    )]
    pub share_mint: Box<Account<'info, Mint>>,
    // The Move `Balance` fields become PDA-seeded token accounts (a
    // single ATA can't hold three separate underlying sub-balances).
    #[account(
        init, payer = admin,
        seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump,
        token::mint = underlying_mint, token::authority = vault,
    )]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(
        init, payer = admin,
        seeds = [PENDING_SEED, vault.key().as_ref()], bump,
        token::mint = underlying_mint, token::authority = vault,
    )]
    pub pending: Box<Account<'info, TokenAccount>>,
    #[account(
        init, payer = admin,
        seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump,
        token::mint = settlement_mint, token::authority = vault,
    )]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    #[account(
        init, payer = admin,
        seeds = [WITHDRAWAL_SEED, vault.key().as_ref()], bump,
        token::mint = underlying_mint, token::authority = vault,
    )]
    pub withdrawal_pool: Box<Account<'info, TokenAccount>>,
    #[account(
        init, payer = admin,
        seeds = [CLAIMABLE_SEED, vault.key().as_ref()], bump,
        token::mint = share_mint, token::authority = vault,
    )]
    pub claimable_shares: Box<Account<'info, TokenAccount>>,
    #[account(
        init, payer = admin,
        seeds = [QUEUED_SEED, vault.key().as_ref()], bump,
        token::mint = share_mint, token::authority = vault,
    )]
    pub queued_shares: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_create_vault(ctx: Context<CreateVault>, salt: u64, config: VaultConfig) -> Result<()> {
    require!(validate_config(&config), VaultError::ConfigInvalid);
    let vault = &mut ctx.accounts.vault;
    vault.admin = ctx.accounts.admin.key();
    vault.underlying_mint = ctx.accounts.underlying_mint.key();
    vault.settlement_mint = ctx.accounts.settlement_mint.key();
    vault.share_mint = ctx.accounts.share_mint.key();
    vault.config = config;
    vault.pending_config = None;
    vault.round = 0;
    vault.phase = Phase::Settling;
    vault.current_bucket = None;
    vault.current_expiry_ms = 0;
    vault.selling_ends_ms = 0;
    vault.open_rfqs = 0;
    vault.open_swap_rfqs = 0;
    vault.positions_head = 0;
    vault.positions_tail = 0;
    vault.round_premium_collected = 0;
    vault.round_swap_settlement_out = 0;
    vault.round_swap_underlying_in = 0;
    vault.paused_deposits = false;
    vault.auction_nonce = 0;
    vault.salt = salt;
    vault.bump = ctx.bumps.vault;

    emit_cpi!(VaultCreated {
        vault: vault.key(),
        underlying_mint: vault.underlying_mint,
        settlement_mint: vault.settlement_mint,
        share_mint: vault.share_mint,
        mgmt_fee_bps_annual: config.mgmt_fee_bps_annual,
        perf_fee_bps: config.perf_fee_bps,
        round_ms: config.round_ms,
        selling_window_ms: config.selling_window_ms,
        min_strike_bps_over_spot: config.min_strike_bps_over_spot,
        max_strike_bps_over_spot: config.max_strike_bps_over_spot,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct VaultAdmin<'info> {
    pub admin: Signer<'info>,
    #[account(mut, constraint = vault.admin == admin.key() @ VaultError::NotAdmin)]
    pub vault: Box<Account<'info, Vault>>,
}

/// Stash a config change; applied at the next finalize so rules can't
/// change mid-round (mirrors `vault::update_config`).
pub fn handle_update_config(ctx: Context<VaultAdmin>, new_config: VaultConfig) -> Result<()> {
    require!(validate_config(&new_config), VaultError::ConfigInvalid);
    let vault = &mut ctx.accounts.vault;
    vault.pending_config = Some(new_config);
    emit_cpi!(VaultConfigUpdated {
        vault: vault.key(),
        round: vault.round,
    });
    Ok(())
}

/// Immediate oracle-feed migration escape hatch (mirrors
/// `vault::update_oracle_feeds`): a vault pinned to a dead feed set is
/// otherwise wedged — the crank that would apply replacement feeds can
/// never resolve the old ones to run. Settling-only, pending kept in sync.
pub fn handle_update_oracle_feeds(
    ctx: Context<VaultAdmin>,
    underlying_feed_id: [u8; 32],
    settlement_feed_id: [u8; 32],
) -> Result<()> {
    let vault = &mut ctx.accounts.vault;
    require!(vault.phase == Phase::Settling, VaultError::WrongPhase);
    vault.config.underlying_feed_id = underlying_feed_id;
    vault.config.settlement_feed_id = settlement_feed_id;
    if let Some(pending) = vault.pending_config.as_mut() {
        pending.underlying_feed_id = underlying_feed_id;
        pending.settlement_feed_id = settlement_feed_id;
    }
    emit_cpi!(VaultConfigUpdated {
        vault: vault.key(),
        round: vault.round,
    });
    Ok(())
}

pub fn handle_set_paused(ctx: Context<VaultAdmin>, paused: bool) -> Result<()> {
    ctx.accounts.vault.paused_deposits = paused;
    emit_cpi!(VaultDepositsPaused {
        vault: ctx.accounts.vault.key(),
        paused,
    });
    Ok(())
}
