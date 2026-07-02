//! Self-describing internal chain id, identical to `sui_bridge::chain_id` (Move)
//! and `ChainId.sol` (Solidity).
//!
//! `internal_id = (family << 27) | local`
//! - top 5 bits  : family (1=Sui, 2=EVM, 3=Solana, 4=Aptos)
//! - low 27 bits : per-family local id
//!
//! The 27-bit local field caps at 134,217,727. For EVM it SHOULD be the native
//! chainId when it fits (HyperEVM testnet = 998 does); otherwise an assigned
//! index, with the registry's native identifier holding the authoritative value.

use thiserror::Error;

pub const FAMILY_SUI: u8 = 1;
pub const FAMILY_EVM: u8 = 2;
pub const FAMILY_SOLANA: u8 = 3;
pub const FAMILY_APTOS: u8 = 4;

const FAMILY_SHIFT: u32 = 27;
const FAMILY_MASK: u32 = 0x1F; // 5 bits
pub const LOCAL_MASK: u32 = 0x07FF_FFFF; // low 27 bits

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChainIdError {
    #[error("unknown family {0}")]
    UnknownFamily(u8),
    #[error("local id {0} exceeds 27-bit ceiling")]
    LocalTooLarge(u32),
}

pub fn is_valid_family(family: u8) -> bool {
    (FAMILY_SUI..=FAMILY_APTOS).contains(&family)
}

/// Compose an internal id from `family` and a 27-bit `local` id.
pub fn encode(family: u8, local: u32) -> Result<u32, ChainIdError> {
    if !is_valid_family(family) {
        return Err(ChainIdError::UnknownFamily(family));
    }
    if local > LOCAL_MASK {
        return Err(ChainIdError::LocalTooLarge(local));
    }
    Ok(((family as u32) << FAMILY_SHIFT) | local)
}

/// The family tag (top 5 bits).
pub fn family(internal_id: u32) -> u8 {
    ((internal_id >> FAMILY_SHIFT) & FAMILY_MASK) as u8
}

/// The per-family local id (low 27 bits).
pub fn local(internal_id: u32) -> u32 {
    internal_id & LOCAL_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_matches_other_chains() {
        let hyper = encode(FAMILY_EVM, 998).unwrap();
        let sui = encode(FAMILY_SUI, 0).unwrap();
        // Same constants the Move + Solidity tests assert.
        assert_eq!(hyper, 268_436_454);
        assert_eq!(sui, 134_217_728);
        assert_eq!(family(hyper), FAMILY_EVM);
        assert_eq!(local(hyper), 998);
        assert_eq!(family(sui), FAMILY_SUI);
        assert_eq!(local(sui), 0);
    }

    #[test]
    fn rejects_bad_family_and_oversized_local() {
        assert_eq!(encode(9, 1), Err(ChainIdError::UnknownFamily(9)));
        assert_eq!(encode(FAMILY_EVM, LOCAL_MASK + 1), Err(ChainIdError::LocalTooLarge(LOCAL_MASK + 1)));
    }
}
