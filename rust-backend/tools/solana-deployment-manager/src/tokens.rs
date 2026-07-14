//! Test SPL mint creation — classic SPL Token (not Token-2022; the
//! programs use `anchor_spl::token`), no freeze authority.
//!
//! The faucet key (solana-gas-station) usually isn't the deployer, so the
//! mint is created with the **payer** as a temporary mint authority, the
//! seed supply is minted to the payer's ATA, and only then is the mint
//! authority handed to the faucet — all in one atomic transaction.

use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::associated_token::spl_associated_token_account;
use anchor_spl::token::spl_token;
use anyhow::{anyhow, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;

/// Seed supply minted to the deployer for MM bootstrap: 1e6 whole tokens.
pub const INITIAL_WHOLE_TOKENS: u64 = 1_000_000;

/// SPL Mint account size (for rent-exemption).
pub const MINT_SPACE: usize = spl_token::state::Mint::LEN;

pub fn initial_supply(decimals: u8) -> u64 {
    INITIAL_WHOLE_TOKENS * 10u64.pow(decimals as u32)
}

/// One transaction's worth of instructions creating a test mint:
/// create_account + initialize_mint2 (authority = payer, freeze = none) +
/// create payer ATA + mint the seed supply + (when the faucet isn't the
/// payer) hand the mint authority to `final_authority`. The mint keypair
/// must co-sign the transaction.
pub fn create_mint_ixs(
    payer: &Pubkey,
    mint: &Pubkey,
    decimals: u8,
    final_authority: &Pubkey,
    rent_lamports: u64,
) -> Result<Vec<Instruction>> {
    let mut ixs = vec![
        solana_system_interface::instruction::create_account(
            payer,
            mint,
            rent_lamports,
            MINT_SPACE as u64,
            &spl_token::ID,
        ),
        spl_token::instruction::initialize_mint2(&spl_token::ID, mint, payer, None, decimals)
            .map_err(|e| anyhow!("building initialize_mint2: {e}"))?,
        spl_associated_token_account::instruction::create_associated_token_account(
            payer,
            payer,
            mint,
            &spl_token::ID,
        ),
        spl_token::instruction::mint_to(
            &spl_token::ID,
            mint,
            &solana_tx::pda::ata(payer, mint),
            payer,
            &[],
            initial_supply(decimals),
        )
        .map_err(|e| anyhow!("building mint_to: {e}"))?,
    ];
    if final_authority != payer {
        ixs.push(
            spl_token::instruction::set_authority(
                &spl_token::ID,
                mint,
                Some(final_authority),
                spl_token::instruction::AuthorityType::MintTokens,
                payer,
                &[],
            )
            .map_err(|e| anyhow!("building set_authority: {e}"))?,
        );
    }
    Ok(ixs)
}

/// Decode an SPL Mint account's decimals; `None` when the account isn't a
/// valid mint owned by the token program (treated as "recreate").
pub fn mint_decimals(owner: &Pubkey, data: &[u8]) -> Option<u8> {
    if *owner != spl_token::ID {
        return None;
    }
    spl_token::state::Mint::unpack(data).ok().map(|m| m.decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_fits_u64_for_all_table_entries() {
        for (_, decimals) in crate::plan::TEST_TOKENS {
            let supply = initial_supply(decimals);
            assert!(supply >= INITIAL_WHOLE_TOKENS);
        }
        // The deepest table entry (TSOL/9): 1e6 × 1e9 = 1e15 « u64::MAX.
        assert_eq!(initial_supply(9), 1_000_000_000_000_000);
    }

    #[test]
    fn set_authority_only_when_faucet_differs() {
        let payer = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let faucet = Pubkey::new_unique();
        assert_eq!(create_mint_ixs(&payer, &mint, 6, &payer, 1).unwrap().len(), 4);
        assert_eq!(create_mint_ixs(&payer, &mint, 6, &faucet, 1).unwrap().len(), 5);
    }
}
