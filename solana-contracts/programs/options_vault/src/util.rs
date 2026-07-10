use anchor_lang::prelude::*;

use crate::state::{Phase, Vault};

pub fn now_ms(clock: &Clock) -> u64 {
    clock.unix_timestamp as u64 * 1000
}

/// Active → Settling once the round's bucket has expired. Rounds that
/// never selected a bucket (`current_expiry_ms == 0`) settle immediately
/// — liveness for zero-deposit / zero-bid rounds and genesis (mirrors
/// `vault::maybe_enter_settling`).
pub fn maybe_enter_settling(vault: &mut Vault, now: u64) {
    if vault.phase == Phase::Active && now >= vault.current_expiry_ms {
        vault.phase = Phase::Settling;
    }
}

/// Vault PDA signer seeds, bound to locals the caller declares.
#[macro_export]
macro_rules! vault_seeds {
    ($vault:expr, $salt:ident, $bump:ident, $seeds:ident, $signer:ident) => {
        let $salt = $vault.salt.to_le_bytes();
        let $bump = [$vault.bump];
        let u_mint = $vault.underlying_mint;
        let s_mint = $vault.settlement_mint;
        let $seeds: [&[u8]; 5] = [
            $crate::state::VAULT_SEED,
            u_mint.as_ref(),
            s_mint.as_ref(),
            &$salt,
            &$bump,
        ];
        let $signer: &[&[&[u8]]] = &[&$seeds];
    };
}
