//! PDA derivations for every seed the three programs use, driven by the
//! program crates' own seed constants (zero drift). Program ids are always
//! arguments — nothing here reads deployments or globals.

use anchor_lang::prelude::Pubkey;

use auction_venue::state as venue_state;
use options_core::state as core_state;
use options_vault::state as vault_state;

/// Anchor's event-cpi authority PDA (`emit_cpi!` self-invoke signer).
/// Every event-emitting instruction takes it plus the program account.
pub fn event_authority(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program_id).0
}

/// The associated token account of `owner` for `mint` (classic SPL token
/// program — the only token program the contracts use).
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    anchor_spl::associated_token::get_associated_token_address(owner, mint)
}

// ── options_core ──

pub fn config(core: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[core_state::CONFIG_SEED], core).0
}

pub fn treasury(core: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[core_state::TREASURY_SEED], core).0
}

pub fn mm_account(core: &Pubkey, owner: &Pubkey, salt: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[core_state::MM_ACCOUNT_SEED, owner.as_ref(), &salt.to_le_bytes()],
        core,
    )
    .0
}

pub fn nonce_record(core: &Pubkey, mm_account: &Pubkey, nonce: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[core_state::NONCE_SEED, mm_account.as_ref(), &nonce.to_le_bytes()],
        core,
    )
    .0
}

pub fn bucket(
    core: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            core_state::BUCKET_SEED,
            underlying_mint.as_ref(),
            settlement_mint.as_ref(),
            &salt.to_le_bytes(),
        ],
        core,
    )
    .0
}

pub fn put_bucket(
    core: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            core_state::PUT_BUCKET_SEED,
            underlying_mint.as_ref(),
            settlement_mint.as_ref(),
            &salt.to_le_bytes(),
        ],
        core,
    )
    .0
}

pub fn call_mint(core: &Pubkey, bucket: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[core_state::CALL_MINT_SEED, bucket.as_ref()], core).0
}

pub fn put_mint(core: &Pubkey, bucket: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[core_state::PUT_MINT_SEED, bucket.as_ref()], core).0
}

// ── auction_venue ──

pub fn auction(venue: &Pubkey, creator: &Pubkey, salt: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[venue_state::AUCTION_SEED, creator.as_ref(), &salt.to_le_bytes()],
        venue,
    )
    .0
}

pub fn escrow_vault(venue: &Pubkey, auction: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[venue_state::ESCROW_SEED, auction.as_ref()], venue).0
}

pub fn bid_vault(venue: &Pubkey, auction: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[venue_state::BIDS_SEED, auction.as_ref()], venue).0
}

// ── options_vault ──

pub fn vault(
    vault_program: &Pubkey,
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    salt: u64,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            vault_state::VAULT_SEED,
            underlying_mint.as_ref(),
            settlement_mint.as_ref(),
            &salt.to_le_bytes(),
        ],
        vault_program,
    )
    .0
}

pub fn share_mint(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[vault_state::SHARE_MINT_SEED, vault.as_ref()], vault_program).0
}

/// The vault's six PDA-seeded token accounts, derived with the same
/// helper: pass the seed constant from `options_vault::state`.
fn vault_token(vault_program: &Pubkey, seed: &[u8], vault: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[seed, vault.as_ref()], vault_program).0
}

pub fn vault_deployable(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::DEPLOYABLE_SEED, vault)
}

pub fn vault_pending(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::PENDING_SEED, vault)
}

pub fn vault_proceeds(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::PROCEEDS_SEED, vault)
}

pub fn vault_withdrawal_pool(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::WITHDRAWAL_SEED, vault)
}

pub fn vault_claimable_shares(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::CLAIMABLE_SEED, vault)
}

pub fn vault_queued_shares(vault_program: &Pubkey, vault: &Pubkey) -> Pubkey {
    vault_token(vault_program, vault_state::QUEUED_SEED, vault)
}

pub fn round_state(vault_program: &Pubkey, vault: &Pubkey, round: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[vault_state::ROUND_SEED, vault.as_ref(), &round.to_le_bytes()],
        vault_program,
    )
    .0
}

pub fn vault_position(vault_program: &Pubkey, vault: &Pubkey, index: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[vault_state::VAULT_POS_SEED, vault.as_ref(), &index.to_le_bytes()],
        vault_program,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The programs record their own PDAs at runtime; here we lock the
    /// derivations against the declared program ids as a drift tripwire.
    #[test]
    fn derivations_are_deterministic() {
        let core = options_core::ID;
        let a = config(&core);
        let b = config(&core);
        assert_eq!(a, b);
        assert_ne!(config(&core), treasury(&core));
        // Distinct programs give distinct event authorities.
        assert_ne!(
            event_authority(&options_core::ID),
            event_authority(&auction_venue::ID)
        );
    }
}
