use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, Token, TokenAccount};

use crate::error::CoreError;
use crate::events::*;
use crate::state::*;
use crate::util::now_ms;

fn assert_scheme_pubkey(scheme: u8, pubkey: &[u8]) -> Result<()> {
    // v1 implements Ed25519 only; the scheme byte is kept so secp256k1/r1
    // can be added append-only (port plan decision #1).
    require!(scheme == SCHEME_ED25519, CoreError::InvalidSigningScheme);
    require!(
        pubkey.len() == ED25519_PUBKEY_LEN,
        CoreError::InvalidPubkeyLength
    );
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(salt: u64)]
pub struct CreateAccount<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        init,
        payer = owner,
        space = 8 + MmAccount::INIT_SPACE,
        seeds = [MM_ACCOUNT_SEED, owner.key().as_ref(), &salt.to_le_bytes()],
        bump
    )]
    pub mm_account: Account<'info, MmAccount>,
    pub system_program: Program<'info, System>,
}

pub fn handle_create_account(
    ctx: Context<CreateAccount>,
    salt: u64,
    signing_scheme: u8,
    signing_pubkey: Vec<u8>,
) -> Result<()> {
    assert_scheme_pubkey(signing_scheme, &signing_pubkey)?;
    let account = &mut ctx.accounts.mm_account;
    account.owner = ctx.accounts.owner.key();
    account.salt = salt;
    account.signing_scheme = signing_scheme;
    account.signing_pubkey = signing_pubkey.clone();
    account.bump = ctx.bumps.mm_account;
    emit_cpi!(AccountCreated {
        account: account.key(),
        owner: account.owner,
        signing_scheme,
        signing_pubkey,
    });
    Ok(())
}

/// Deposits are permissionless, like Sui's `account::deposit` — anyone can
/// fund any account. Balances live in ATAs owned by the account PDA.
#[event_cpi]
#[derive(Accounts)]
pub struct DepositToAccount<'info> {
    #[account(mut)]
    pub depositor: Signer<'info>,
    pub mm_account: Account<'info, MmAccount>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(mut, token::mint = mint)]
    pub from_token: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = depositor,
        associated_token::mint = mint,
        associated_token::authority = mm_account,
    )]
    pub account_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn handle_account_deposit(ctx: Context<DepositToAccount>, amount: u64) -> Result<()> {
    require!(amount > 0, CoreError::ZeroAmount);
    token::transfer(
        CpiContext::new(
            token::ID,
            token::Transfer {
                from: ctx.accounts.from_token.to_account_info(),
                to: ctx.accounts.account_token.to_account_info(),
                authority: ctx.accounts.depositor.to_account_info(),
            },
        ),
        amount,
    )?;
    emit_cpi!(AccountDeposit {
        account: ctx.accounts.mm_account.key(),
        mint: ctx.accounts.mint.key(),
        amount,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawFromAccount<'info> {
    pub owner: Signer<'info>,
    #[account(
        seeds = [MM_ACCOUNT_SEED, mm_account.owner.as_ref(), &mm_account.salt.to_le_bytes()],
        bump = mm_account.bump,
        constraint = mm_account.owner == owner.key() @ CoreError::NotOwner
    )]
    pub mm_account: Account<'info, MmAccount>,
    pub mint: Box<Account<'info, Mint>>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = mm_account,
    )]
    pub account_token: Box<Account<'info, TokenAccount>>,
    #[account(mut, token::mint = mint)]
    pub to_token: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

pub fn handle_account_withdraw(ctx: Context<WithdrawFromAccount>, amount: u64) -> Result<()> {
    require!(
        ctx.accounts.account_token.amount >= amount,
        CoreError::InsufficientAccountBalance
    );
    let mm = &ctx.accounts.mm_account;
    let salt = mm.salt.to_le_bytes();
    let bump = [mm.bump];
    let signer_seeds: &[&[&[u8]]] = &[&[MM_ACCOUNT_SEED, mm.owner.as_ref(), &salt, &bump]];
    token::transfer(
        CpiContext::new_with_signer(
            token::ID,
            token::Transfer {
                from: ctx.accounts.account_token.to_account_info(),
                to: ctx.accounts.to_token.to_account_info(),
                authority: ctx.accounts.mm_account.to_account_info(),
            },
            signer_seeds,
        ),
        amount,
    )?;
    emit_cpi!(AccountWithdraw {
        account: ctx.accounts.mm_account.key(),
        mint: ctx.accounts.mint.key(),
        amount,
    });
    Ok(())
}

#[event_cpi]
#[derive(Accounts)]
pub struct RotateSigningKey<'info> {
    pub owner: Signer<'info>,
    #[account(
        mut,
        constraint = mm_account.owner == owner.key() @ CoreError::NotOwner
    )]
    pub mm_account: Account<'info, MmAccount>,
}

pub fn handle_rotate_signing_key(
    ctx: Context<RotateSigningKey>,
    new_scheme: u8,
    new_pubkey: Vec<u8>,
) -> Result<()> {
    assert_scheme_pubkey(new_scheme, &new_pubkey)?;
    let account = &mut ctx.accounts.mm_account;
    account.signing_scheme = new_scheme;
    account.signing_pubkey = new_pubkey.clone();
    emit_cpi!(SigningKeyRotated {
        account: account.key(),
        new_scheme,
        new_pubkey,
    });
    Ok(())
}

/// Permissionless nonce pruning (mirrors `account::prune_nonce`): anyone
/// may close an expired nonce record; the rent refund to the caller is the
/// incentive (the analog of Sui's storage rebate).
#[derive(Accounts)]
pub struct PruneNonce<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,
    #[account(mut, close = caller)]
    pub nonce_record: Account<'info, NonceRecord>,
}

pub fn handle_prune_nonce(ctx: Context<PruneNonce>) -> Result<()> {
    let clock = Clock::get()?;
    require!(
        now_ms(&clock) > ctx.accounts.nonce_record.valid_until_ms,
        CoreError::NonceStillValid
    );
    Ok(())
}
