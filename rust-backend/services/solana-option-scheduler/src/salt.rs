//! Deterministic PDA salts.
//!
//! `salt` (the bucket/vault PDA seed the programs take) is derived by
//! hashing the economic identity of the account, truncated to u64 — so a
//! re-run of the same roll derives the same PDA and a double-submit
//! collides on-chain (`already in use`, classified Benign) instead of
//! duplicating. This complements the DB's partial UNIQUE index.

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;

use crate::roller::ProductType;

/// Bucket salt: first 8 bytes of
/// `sha256(underlying_mint ‖ settlement_mint ‖ expiry_ms LE ‖ strike LE ‖
/// strike_scale ‖ product_type)` as a little-endian u64.
pub fn bucket_salt(
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    expiry_ms: u64,
    strike: u128,
    strike_scale: u8,
    product_type: ProductType,
) -> u64 {
    let mut h = Sha256::new();
    h.update(underlying_mint.as_ref());
    h.update(settlement_mint.as_ref());
    h.update(expiry_ms.to_le_bytes());
    h.update(strike.to_le_bytes());
    h.update([strike_scale]);
    h.update(product_type.as_str().as_bytes());
    truncate(h.finalize().as_slice())
}

/// Vault salt: first 8 bytes of
/// `sha256(underlying_mint ‖ settlement_mint ‖ round_ms LE ‖ generation LE)`
/// as a little-endian u64. One vault per (pair, cadence, generation) by
/// construction: `generation` is the replacement counter (number of retired
/// vaults for the pair+cadence, stamped on the scheduler_vaults row at claim
/// time), so replacing a paused vault derives a NEW PDA instead of colliding
/// with — and re-adopting — the decommissioned one, while a failed create's
/// retry (same generation) still resolves idempotently by salt collision.
pub fn vault_salt(
    underlying_mint: &Pubkey,
    settlement_mint: &Pubkey,
    round_ms: u64,
    generation: u64,
) -> u64 {
    let mut h = Sha256::new();
    h.update(underlying_mint.as_ref());
    h.update(settlement_mint.as_ref());
    h.update(round_ms.to_le_bytes());
    h.update(generation.to_le_bytes());
    truncate(h.finalize().as_slice())
}

fn truncate(digest: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mints() -> (Pubkey, Pubkey) {
        (Pubkey::new_from_array([7u8; 32]), Pubkey::new_from_array([9u8; 32]))
    }

    #[test]
    fn bucket_salt_is_deterministic() {
        let (u, s) = mints();
        let a = bucket_salt(&u, &s, 1_760_000_000_000, 65_000, 2, ProductType::Call);
        let b = bucket_salt(&u, &s, 1_760_000_000_000, 65_000, 2, ProductType::Call);
        assert_eq!(a, b);
    }

    #[test]
    fn bucket_salt_is_sensitive_to_every_field() {
        let (u, s) = mints();
        let base = bucket_salt(&u, &s, 1_000, 65_000, 2, ProductType::Call);
        assert_ne!(base, bucket_salt(&s, &u, 1_000, 65_000, 2, ProductType::Call), "mint order");
        assert_ne!(base, bucket_salt(&u, &s, 1_001, 65_000, 2, ProductType::Call), "expiry");
        assert_ne!(base, bucket_salt(&u, &s, 1_000, 65_001, 2, ProductType::Call), "strike");
        assert_ne!(base, bucket_salt(&u, &s, 1_000, 65_000, 3, ProductType::Call), "scale");
        assert_ne!(base, bucket_salt(&u, &s, 1_000, 65_000, 2, ProductType::Put), "product");
    }

    #[test]
    fn bucket_salt_matches_manual_derivation() {
        // Lock the byte layout (LE ints, product tag string) so a refactor
        // can't silently re-derive different PDAs for existing buckets.
        let (u, s) = mints();
        let mut h = Sha256::new();
        h.update(u.as_ref());
        h.update(s.as_ref());
        h.update(1_760_000_000_000u64.to_le_bytes());
        h.update(65_000u128.to_le_bytes());
        h.update([2u8]);
        h.update(b"put");
        let expected = u64::from_le_bytes(h.finalize()[..8].try_into().unwrap());
        assert_eq!(
            bucket_salt(&u, &s, 1_760_000_000_000, 65_000, 2, ProductType::Put),
            expected
        );
    }

    #[test]
    fn vault_salt_is_deterministic_and_cadence_scoped() {
        let (u, s) = mints();
        const WEEK: u64 = 604_800_000;
        const HOUR: u64 = 3_600_000;
        assert_eq!(vault_salt(&u, &s, WEEK, 0), vault_salt(&u, &s, WEEK, 0));
        assert_ne!(vault_salt(&u, &s, WEEK, 0), vault_salt(&u, &s, HOUR, 0));
        assert_ne!(vault_salt(&u, &s, WEEK, 0), vault_salt(&s, &u, WEEK, 0));
    }

    #[test]
    fn vault_salt_generation_forces_a_new_pda() {
        // Retire → recreate: generation 1 must derive a different salt (and
        // therefore a different vault PDA) from generation 0, or the
        // replacement create would collide with the paused vault and the
        // adopt path would loop it back in.
        let (u, s) = mints();
        const WEEK: u64 = 604_800_000;
        let gen0 = vault_salt(&u, &s, WEEK, 0);
        let gen1 = vault_salt(&u, &s, WEEK, 1);
        assert_ne!(gen0, gen1);
        let pda0 = solana_tx::pda::vault(&options_vault::ID, &u, &s, gen0);
        let pda1 = solana_tx::pda::vault(&options_vault::ID, &u, &s, gen1);
        assert_ne!(pda0, pda1);
    }

    #[test]
    fn vault_salt_matches_manual_derivation() {
        // Lock the byte layout (mints ‖ round_ms LE ‖ generation LE u64) so
        // a refactor can't silently re-derive different PDAs for existing
        // vaults.
        let (u, s) = mints();
        let mut h = Sha256::new();
        h.update(u.as_ref());
        h.update(s.as_ref());
        h.update(604_800_000u64.to_le_bytes());
        h.update(3u64.to_le_bytes());
        let expected = u64::from_le_bytes(h.finalize()[..8].try_into().unwrap());
        assert_eq!(vault_salt(&u, &s, 604_800_000, 3), expected);
    }
}
