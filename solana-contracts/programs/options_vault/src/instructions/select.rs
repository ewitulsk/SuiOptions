use anchor_lang::prelude::*;

use crate::error::VaultError;
use crate::vault_seeds;
use crate::events::*;
use crate::oracle;
use crate::state::*;
use crate::util::now_ms;

/// Pick the round's bucket (mirrors `vault::select_bucket`): expiry inside
/// the configured lead window, strike inside the Pyth band. The band
/// bounds — not eliminates — keeper discretion; `min_reserve_premium_bps`
/// is the real per-slice loss bound.
#[event_cpi]
#[derive(Accounts)]
pub struct SelectBucket<'info> {
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    pub bucket: Box<Account<'info, options_core::state::Bucket>>,
    /// CHECK: Pyth PriceUpdateV2, fully validated in oracle::spot_cross.
    pub underlying_price: UncheckedAccount<'info>,
    /// CHECK: Pyth PriceUpdateV2, fully validated in oracle::spot_cross.
    pub settlement_price: UncheckedAccount<'info>,
}

pub fn handle_select_bucket(ctx: Context<SelectBucket>) -> Result<()> {
    let clock = Clock::get()?;
    let now = now_ms(&clock);
    let vault = &ctx.accounts.vault;
    let bucket = &ctx.accounts.bucket;

    require!(vault.phase == Phase::Active, VaultError::WrongPhase);
    require!(
        vault.current_bucket.is_none(),
        VaultError::BucketAlreadySelected
    );
    require!(!bucket.invalidated, VaultError::BucketInvalidated);
    require!(
        bucket.underlying_mint == vault.underlying_mint
            && bucket.settlement_mint == vault.settlement_mint,
        VaultError::AccountMismatch
    );

    let expiry = bucket.expiry_ms;
    require!(
        expiry >= now + vault.config.min_expiry_lead_ms
            && expiry <= now + vault.config.max_expiry_lead_ms,
        VaultError::ExpiryOutOfBand
    );

    let (spot, spot_scale) = oracle::spot_cross(
        &ctx.accounts.underlying_price,
        &ctx.accounts.settlement_price,
        &vault.config,
        clock.unix_timestamp as u64,
    )?;

    // Strike band, cross-multiplied so the comparison is exact at both
    // scales: strike/10^ss ⋛ spot/10^os × (1 + bps/10⁴). Move computed in
    // u256; checked u128 covers every realistic magnitude and overflow
    // degrades to a clean error.
    let bps = options_math::BPS_DENOM;
    let pow_ss = options_math::pow10(spot_scale).ok_or(VaultError::MathOverflow)?;
    let pow_bs = options_math::pow10(bucket.strike_scale).ok_or(VaultError::MathOverflow)?;
    let lhs = bucket
        .strike
        .checked_mul(pow_ss)
        .and_then(|v| v.checked_mul(bps))
        .ok_or(VaultError::MathOverflow)?;
    let rhs_base = spot.checked_mul(pow_bs).ok_or(VaultError::MathOverflow)?;
    let rhs_min = rhs_base
        .checked_mul(bps + vault.config.min_strike_bps_over_spot as u128)
        .ok_or(VaultError::MathOverflow)?;
    let rhs_max = rhs_base
        .checked_mul(bps + vault.config.max_strike_bps_over_spot as u128)
        .ok_or(VaultError::MathOverflow)?;
    require!(lhs >= rhs_min && lhs <= rhs_max, VaultError::StrikeOutOfBand);

    let vault = &mut ctx.accounts.vault;
    vault.current_bucket = Some(bucket.key());
    vault.current_expiry_ms = expiry;
    // Cap the selling window so the last possible auction still has room
    // for its full duration + extensions + the settle buffer.
    let auction_room = vault.config.rfq_duration_ms
        + vault.config.rfq_max_extension_ms
        + SETTLE_BUFFER_MS;
    let hard_cap = expiry - auction_room;
    vault.selling_ends_ms = (now + vault.config.selling_window_ms).min(hard_cap);

    emit_cpi!(VaultBucketSelected {
        vault: vault.key(),
        round: vault.round,
        bucket: bucket.key(),
        strike: bucket.strike,
        strike_scale: bucket.strike_scale,
        expiry_ms: expiry,
        selling_ends_ms: vault.selling_ends_ms,
        spot,
        spot_scale,
    });
    Ok(())
}

/// Redeem the next position in the FIFO — one per call, bounded compute
/// (mirrors `vault::crank_redeem`). The first call after expiry flips the
/// phase to Settling. CPIs core's `redeem_position` with the vault PDA as
/// the position owner; proceeds land in the deployable/proceeds vaults.
#[event_cpi]
#[derive(Accounts)]
pub struct CrankRedeem<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    /// CHECK: core re-validates the bucket fully.
    #[account(mut, constraint = vault.current_bucket == Some(bucket.key()) @ VaultError::BucketNotSelected)]
    pub bucket: UncheckedAccount<'info>,
    /// The FIFO head entry; closed after redemption, rent to the cranker.
    #[account(
        mut,
        close = cranker,
        seeds = [VAULT_POS_SEED, vault.key().as_ref(), &vault.positions_head.to_le_bytes()],
        bump = vault_position.bump,
    )]
    pub vault_position: Box<Account<'info, VaultPosition>>,
    /// CHECK: the core Position at the FIFO head, pinned by vault_position.
    #[account(mut, address = vault_position.position @ VaultError::WrongIndex)]
    pub position: UncheckedAccount<'info>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, anchor_spl::token::TokenAccount>>,
    #[account(mut, seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, anchor_spl::token::TokenAccount>>,
    /// CHECK: the bucket's vaults; core enforces their identity.
    #[account(mut)]
    pub bucket_underlying_vault: UncheckedAccount<'info>,
    /// CHECK: core enforces.
    #[account(mut)]
    pub bucket_settlement_vault: UncheckedAccount<'info>,
    /// CHECK: core's event authority PDA.
    pub core_event_authority: UncheckedAccount<'info>,
    pub core_program: Program<'info, options_core::program::OptionsCore>,
    pub token_program: Program<'info, anchor_spl::token::Token>,
}

pub fn handle_crank_redeem(ctx: Context<CrankRedeem>) -> Result<()> {
    let now = now_ms(&Clock::get()?);
    let vault = &mut ctx.accounts.vault;
    crate::util::maybe_enter_settling(vault, now);
    require!(vault.phase == Phase::Settling, VaultError::WrongPhase);
    require!(
        vault.positions_head < vault.positions_tail,
        VaultError::PositionsPending
    );

    let u_before = ctx.accounts.deployable.amount;
    let s_before = ctx.accounts.proceeds.amount;

    let vault = &ctx.accounts.vault;
    vault_seeds!(vault, salt, bump, seeds, signer_seeds);
    options_core::cpi::redeem_position(CpiContext::new_with_signer(
        options_core::ID,
        options_core::cpi::accounts::RedeemPosition {
            redeemer: ctx.accounts.vault.to_account_info(),
            bucket: ctx.accounts.bucket.to_account_info(),
            position: ctx.accounts.position.to_account_info(),
            redeemer_underlying: ctx.accounts.deployable.to_account_info(),
            redeemer_settlement: ctx.accounts.proceeds.to_account_info(),
            underlying_vault: ctx.accounts.bucket_underlying_vault.to_account_info(),
            settlement_vault: ctx.accounts.bucket_settlement_vault.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            event_authority: ctx.accounts.core_event_authority.to_account_info(),
            program: ctx.accounts.core_program.to_account_info(),
        },
        signer_seeds,
    ))?;

    ctx.accounts.deployable.reload()?;
    ctx.accounts.proceeds.reload()?;
    let vault = &mut ctx.accounts.vault;
    vault.positions_head += 1;

    emit_cpi!(VaultPositionRedeemed {
        vault: vault.key(),
        round: vault.round,
        position: ctx.accounts.position.key(),
        underlying: ctx.accounts.deployable.amount - u_before,
        settlement: ctx.accounts.proceeds.amount - s_before,
    });
    Ok(())
}
