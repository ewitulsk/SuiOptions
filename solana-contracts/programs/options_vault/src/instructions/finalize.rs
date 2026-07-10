use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::VaultError;
use crate::vault_seeds;
use crate::events::*;
use crate::oracle;
use crate::state::*;
use crate::util::{maybe_enter_settling, now_ms};

/// The accounting heart (mirrors `vault::finalize_round`, which mirrors
/// `vault-sim::ledger` unit-for-unit): lock pps, charge fees on
/// profitable rounds (mgmt-first / perf-clamped), process queues
/// (withdrawals then deposits at pps[round]), and activate the next
/// round. Fees go to options_core's treasury, like Sui's protocol
/// `Treasury`.
#[event_cpi]
#[derive(Accounts)]
pub struct FinalizeRound<'info> {
    #[account(mut)]
    pub cranker: Signer<'info>,
    #[account(mut)]
    pub vault: Box<Account<'info, Vault>>,
    #[account(mut, address = vault.share_mint)]
    pub share_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [DEPLOYABLE_SEED, vault.key().as_ref()], bump)]
    pub deployable: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [PENDING_SEED, vault.key().as_ref()], bump)]
    pub pending: Box<Account<'info, TokenAccount>>,
    #[account(seeds = [PROCEEDS_SEED, vault.key().as_ref()], bump)]
    pub proceeds: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [WITHDRAWAL_SEED, vault.key().as_ref()], bump)]
    pub withdrawal_pool: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [CLAIMABLE_SEED, vault.key().as_ref()], bump)]
    pub claimable_shares: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [QUEUED_SEED, vault.key().as_ref()], bump)]
    pub queued_shares: Box<Account<'info, TokenAccount>>,
    /// pps[round], set exactly once here.
    #[account(
        init,
        payer = cranker,
        space = 8 + RoundState::INIT_SPACE,
        seeds = [ROUND_SEED, vault.key().as_ref(), &vault.round.to_le_bytes()],
        bump
    )]
    pub round_state: Box<Account<'info, RoundState>>,
    /// pps[round − 1]; required for every round after genesis. Identity
    /// pinned in the handler (vault + round fields) — an optional account
    /// can't carry a sibling-referencing seeds constraint through the IDL
    /// build.
    pub prev_round_state: Option<Box<Account<'info, RoundState>>>,
    /// Fee destination: core treasury's underlying token account
    /// (ownership verified in the handler).
    #[account(mut, token::mint = vault.underlying_mint)]
    pub core_treasury_token: Box<Account<'info, TokenAccount>>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub underlying_price: UncheckedAccount<'info>,
    /// CHECK: Pyth PriceUpdateV2, validated in oracle::spot_cross.
    pub settlement_price: UncheckedAccount<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handle_finalize_round(ctx: Context<FinalizeRound>) -> Result<()> {
    let clock = Clock::get()?;
    let now = now_ms(&clock);
    let vault = &mut ctx.accounts.vault;
    maybe_enter_settling(vault, now);
    require!(vault.phase == Phase::Settling, VaultError::WrongPhase);
    require!(
        vault.positions_head == vault.positions_tail,
        VaultError::PositionsPending
    );
    require!(
        vault.open_rfqs == 0 && vault.open_swap_rfqs == 0,
        VaultError::RfqsOpen
    );

    let vault = &ctx.accounts.vault;
    let (spot, spot_scale) = oracle::spot_cross(
        &ctx.accounts.underlying_price,
        &ctx.accounts.settlement_price,
        &vault.config,
        clock.unix_timestamp as u64,
    )?;

    // Proceeds policy: under hold-premium the residual settlement is
    // valued at the Pyth cross; otherwise it must have been swapped.
    let residual_s = ctx.accounts.proceeds.amount;
    let valued_s = if vault.config.hold_premium_in_settlement {
        options_math::settlement_to_underlying(residual_s, spot, spot_scale)
            .ok_or(VaultError::MathOverflow)?
    } else {
        require!(residual_s == 0, VaultError::ProceedsUnswapped);
        0
    };

    let round = vault.round;
    let aum = ctx.accounts.deployable.amount + valued_s;
    let shares = ctx.accounts.share_mint.supply;
    let pps_prev = if round == 0 {
        options_math::PPS_SCALE
    } else {
        let prev = ctx
            .accounts
            .prev_round_state
            .as_ref()
            .ok_or(VaultError::RoundNotFinalized)?;
        require!(
            prev.round == round - 1 && prev.vault == vault.key(),
            VaultError::RoundNotFinalized
        );
        prev.pps
    };
    let pps_gross = if shares == 0 {
        pps_prev
    } else {
        (aum as u128) * options_math::PPS_SCALE / (shares as u128)
    };

    // Premium in underlying terms, for the perf fee: at the round's
    // realized swap rate, or the Pyth cross under hold-premium.
    let premium_s = vault.round_premium_collected;
    let premium_u = if vault.config.hold_premium_in_settlement {
        options_math::settlement_to_underlying(premium_s, spot, spot_scale)
            .ok_or(VaultError::MathOverflow)?
    } else if vault.round_swap_settlement_out > 0 {
        ((premium_s as u128) * (vault.round_swap_underlying_in as u128)
            / (vault.round_swap_settlement_out as u128)) as u64
    } else {
        0
    };

    // Fees only on profitable rounds; mgmt first, perf absorbs the
    // profit cap (mirrors vault-sim::ledger).
    let (mgmt_fee, perf_fee) = if pps_gross > pps_prev {
        let mgmt = (aum as u128) * (vault.config.mgmt_fee_bps_annual as u128)
            * (vault.config.round_ms as u128)
            / (options_math::BPS_DENOM * YEAR_MS);
        let perf =
            (premium_u as u128) * (vault.config.perf_fee_bps as u128) / options_math::BPS_DENOM;
        let baseline = (shares as u128) * pps_prev / options_math::PPS_SCALE;
        let profit = (aum as u128) - baseline;
        let mgmt_charged = mgmt.min(profit);
        let perf_charged = perf.min(profit - mgmt_charged);
        (mgmt_charged as u64, perf_charged as u64)
    } else {
        (0, 0)
    };
    let fees = mgmt_fee + perf_fee;

    vault_seeds!(vault, vsalt, vbump, seeds, signer_seeds);
    if fees > 0 {
        // Verify the destination really is core's treasury.
        let core_treasury =
            Pubkey::find_program_address(&[b"treasury"], &options_core::ID).0;
        require!(
            ctx.accounts.core_treasury_token.owner == core_treasury,
            VaultError::AccountMismatch
        );
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.deployable.to_account_info(),
                    to: ctx.accounts.core_treasury_token.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            fees,
        )?;
    }
    emit_cpi!(VaultFeesCharged {
        vault: vault.key(),
        round,
        mgmt_fee,
        perf_fee,
    });

    // Lock the round price.
    let net_aum = aum - fees;
    let pps = if shares == 0 {
        pps_prev
    } else {
        (net_aum as u128) * options_math::PPS_SCALE / (shares as u128)
    };
    let round_state = &mut ctx.accounts.round_state;
    round_state.vault = vault.key();
    round_state.round = round;
    round_state.pps = pps;
    round_state.bump = ctx.bumps.round_state;

    // Queues, in order: withdrawals first, then deposits, both at
    // pps[round].
    let shares_burned = ctx.accounts.queued_shares.amount;
    let withdrawals_owed =
        options_math::amount_for_shares(shares_burned, pps).ok_or(VaultError::MathOverflow)?;
    // Withdrawals pay underlying: under hold-premium a large settlement
    // carry could leave deployable short — force a fill first.
    ctx.accounts.deployable.reload()?;
    require!(
        ctx.accounts.deployable.amount >= withdrawals_owed,
        VaultError::ProceedsUnswapped
    );
    if withdrawals_owed > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.deployable.to_account_info(),
                    to: ctx.accounts.withdrawal_pool.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            withdrawals_owed,
        )?;
    }
    if shares_burned > 0 {
        token::burn(
            CpiContext::new_with_signer(
                token::ID,
                token::Burn {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    from: ctx.accounts.queued_shares.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            shares_burned,
        )?;
    }

    let deposits_processed = ctx.accounts.pending.amount;
    let shares_minted =
        options_math::shares_for_amount(deposits_processed, pps).ok_or(VaultError::MathOverflow)?;
    if deposits_processed > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token::ID,
                token::Transfer {
                    from: ctx.accounts.pending.to_account_info(),
                    to: ctx.accounts.deployable.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            deposits_processed,
        )?;
    }
    if shares_minted > 0 {
        // Minted in aggregate so unclaimed depositors earn their rounds'
        // P&L; per-receipt floor dust stays with the vault.
        token::mint_to(
            CpiContext::new_with_signer(
                token::ID,
                token::MintTo {
                    mint: ctx.accounts.share_mint.to_account_info(),
                    to: ctx.accounts.claimable_shares.to_account_info(),
                    authority: ctx.accounts.vault.to_account_info(),
                },
                signer_seeds,
            ),
            shares_minted,
        )?;
    }

    // Activate the next round.
    let vault = &mut ctx.accounts.vault;
    vault.round = round + 1;
    vault.phase = Phase::Active;
    vault.current_bucket = None;
    vault.current_expiry_ms = 0;
    vault.selling_ends_ms = 0;
    vault.positions_head = 0;
    vault.positions_tail = 0;
    vault.round_premium_collected = 0;
    vault.round_swap_settlement_out = 0;
    vault.round_swap_underlying_in = 0;
    if let Some(pending_config) = vault.pending_config.take() {
        vault.config = pending_config;
    }

    emit_cpi!(VaultConfigApplied {
        vault: vault.key(),
        round,
        mgmt_fee_bps_annual: vault.config.mgmt_fee_bps_annual,
        perf_fee_bps: vault.config.perf_fee_bps,
        round_ms: vault.config.round_ms,
        selling_window_ms: vault.config.selling_window_ms,
        min_strike_bps_over_spot: vault.config.min_strike_bps_over_spot,
        max_strike_bps_over_spot: vault.config.max_strike_bps_over_spot,
    });
    emit_cpi!(VaultRoundFinalized {
        vault: vault.key(),
        round,
        pps,
        aum,
        shares,
        premium_s,
        premium_u,
        withdrawals_owed,
        shares_burned,
        deposits_processed,
        shares_minted,
    });
    Ok(())
}
