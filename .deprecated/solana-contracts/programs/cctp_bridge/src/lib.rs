//! Protocol entry point for outbound Circle CCTP v1 transfers (Solana side).
//!
//! Wraps Circle's TokenMessengerMinter `deposit_for_burn` in a CPI so all
//! bridge traffic flows through this program (own event, future fee/control
//! hooks). Circle's v1 programs are Anchor 0.28, so the instruction is built
//! manually (discriminator + borsh params) rather than via their crate to
//! avoid an anchor-lang version conflict.
//!
//! Circle program IDs are identical on devnet and mainnet.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke;
use anchor_spl::token::{Mint, Token, TokenAccount};

declare_id!("77R21RcDcQuhWPkTNHh7BeUgBstF2Nmsysp86QpZam86");

/// Circle CCTP v1 TokenMessengerMinter (devnet + mainnet).
pub const TOKEN_MESSENGER_MINTER_ID: Pubkey =
    pubkey!("CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3");
/// Circle CCTP v1 MessageTransmitter (devnet + mainnet).
pub const MESSAGE_TRANSMITTER_ID: Pubkey =
    pubkey!("CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd");

/// sha256("global:deposit_for_burn")[..8] — Circle's Anchor 0.28 discriminator.
const DEPOSIT_FOR_BURN_DISCRIMINATOR: [u8; 8] = [215, 60, 61, 46, 114, 55, 128, 176];

#[program]
pub mod cctp_bridge {
    use super::*;

    /// Burn-side entry point: burns `amount` of the user's USDC via Circle's
    /// `deposit_for_burn` CPI and emits `BridgeInitiated`.
    ///
    /// `mint_recipient` is the bytes32 recipient on the destination domain
    /// (for Sui: the recipient's Sui address).
    pub fn deposit_for_burn(
        ctx: Context<DepositForBurn>,
        amount: u64,
        destination_domain: u32,
        mint_recipient: Pubkey,
    ) -> Result<()> {
        let mut data = Vec::with_capacity(8 + 8 + 4 + 32);
        data.extend_from_slice(&DEPOSIT_FOR_BURN_DISCRIMINATOR);
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&destination_domain.to_le_bytes());
        data.extend_from_slice(mint_recipient.as_ref());

        // Account order mirrors Circle's DepositForBurnContext, with the
        // event-CPI pair (event_authority, program) appended by Anchor 0.28.
        let ix = Instruction {
            program_id: TOKEN_MESSENGER_MINTER_ID,
            accounts: vec![
                AccountMeta::new_readonly(ctx.accounts.owner.key(), true),
                AccountMeta::new(ctx.accounts.owner.key(), true), // event_rent_payer
                AccountMeta::new_readonly(ctx.accounts.sender_authority_pda.key(), false),
                AccountMeta::new(ctx.accounts.burn_token_account.key(), false),
                AccountMeta::new(ctx.accounts.message_transmitter.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_messenger.key(), false),
                AccountMeta::new_readonly(ctx.accounts.remote_token_messenger.key(), false),
                AccountMeta::new_readonly(ctx.accounts.token_minter.key(), false),
                AccountMeta::new(ctx.accounts.local_token.key(), false),
                AccountMeta::new(ctx.accounts.burn_token_mint.key(), false),
                AccountMeta::new(ctx.accounts.message_sent_event_data.key(), true),
                AccountMeta::new_readonly(ctx.accounts.message_transmitter_program.key(), false),
                AccountMeta::new_readonly(
                    ctx.accounts.token_messenger_minter_program.key(),
                    false,
                ),
                AccountMeta::new_readonly(ctx.accounts.token_program.key(), false),
                AccountMeta::new_readonly(ctx.accounts.system_program.key(), false),
                AccountMeta::new_readonly(
                    ctx.accounts.token_messenger_minter_event_authority.key(),
                    false,
                ),
                AccountMeta::new_readonly(
                    ctx.accounts.token_messenger_minter_program.key(),
                    false,
                ),
            ],
            data,
        };

        invoke(
            &ix,
            &[
                ctx.accounts.owner.to_account_info(),
                ctx.accounts.sender_authority_pda.to_account_info(),
                ctx.accounts.burn_token_account.to_account_info(),
                ctx.accounts.message_transmitter.to_account_info(),
                ctx.accounts.token_messenger.to_account_info(),
                ctx.accounts.remote_token_messenger.to_account_info(),
                ctx.accounts.token_minter.to_account_info(),
                ctx.accounts.local_token.to_account_info(),
                ctx.accounts.burn_token_mint.to_account_info(),
                ctx.accounts.message_sent_event_data.to_account_info(),
                ctx.accounts.message_transmitter_program.to_account_info(),
                ctx.accounts.token_messenger_minter_program.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.token_messenger_minter_event_authority.to_account_info(),
            ],
        )?;

        emit_cpi!(BridgeInitiated {
            sender: ctx.accounts.owner.key(),
            amount,
            destination_domain,
            mint_recipient,
            burn_token: ctx.accounts.burn_token_mint.key(),
        });

        Ok(())
    }
}

#[event_cpi]
#[derive(Accounts)]
pub struct DepositForBurn<'info> {
    /// Owner of the burn token account; also pays Circle's event-account rent.
    #[account(mut)]
    pub owner: Signer<'info>,

    /// CHECK: Circle's sender_authority PDA — validated by the CPI.
    pub sender_authority_pda: UncheckedAccount<'info>,

    #[account(mut, constraint = burn_token_account.owner == owner.key())]
    pub burn_token_account: Box<Account<'info, TokenAccount>>,

    /// CHECK: Circle MessageTransmitter state — validated by the CPI.
    #[account(mut)]
    pub message_transmitter: UncheckedAccount<'info>,

    /// CHECK: Circle TokenMessenger state — validated by the CPI.
    pub token_messenger: UncheckedAccount<'info>,

    /// CHECK: Circle RemoteTokenMessenger for the destination domain — validated by the CPI.
    pub remote_token_messenger: UncheckedAccount<'info>,

    /// CHECK: Circle TokenMinter state — validated by the CPI.
    pub token_minter: UncheckedAccount<'info>,

    /// CHECK: Circle LocalToken PDA for the burn mint — validated by the CPI.
    #[account(mut)]
    pub local_token: UncheckedAccount<'info>,

    #[account(mut)]
    pub burn_token_mint: Box<Account<'info, Mint>>,

    /// Fresh keypair Circle stores the MessageSent event data in.
    #[account(mut)]
    pub message_sent_event_data: Signer<'info>,

    /// CHECK: pinned to Circle's MessageTransmitter program.
    #[account(address = MESSAGE_TRANSMITTER_ID)]
    pub message_transmitter_program: UncheckedAccount<'info>,

    /// CHECK: pinned to Circle's TokenMessengerMinter program.
    #[account(address = TOKEN_MESSENGER_MINTER_ID)]
    pub token_messenger_minter_program: UncheckedAccount<'info>,

    /// CHECK: Circle's event authority PDA (["__event_authority"] of TokenMessengerMinter).
    pub token_messenger_minter_event_authority: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,

    pub system_program: Program<'info, System>,
}

#[event]
pub struct BridgeInitiated {
    pub sender: Pubkey,
    pub amount: u64,
    pub destination_domain: u32,
    pub mint_recipient: Pubkey,
    pub burn_token: Pubkey,
}
