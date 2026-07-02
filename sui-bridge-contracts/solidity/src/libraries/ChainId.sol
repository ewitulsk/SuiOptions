// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title ChainId
/// @notice Self-describing internal chain id, identical to the Move
///         `sui_bridge::chain_id` module so both sides agree on the values in a
///         signed message.
///
///         internal_id = (family << 27) | local
///         - top 5 bits  : family (1=Sui, 2=EVM, 3=Solana, 4=Aptos)
///         - low 27 bits : per-family local id
///
///         The 27-bit local field caps at 134,217,727. For EVM it SHOULD be the
///         native chainId when it fits (HyperEVM testnet = 998 does); otherwise
///         an assigned index, with the registry's `nativeIdentifier` holding the
///         authoritative value.
library ChainId {
    uint8 internal constant FAMILY_SUI = 1;
    uint8 internal constant FAMILY_EVM = 2;
    uint8 internal constant FAMILY_SOLANA = 3;
    uint8 internal constant FAMILY_APTOS = 4;

    uint8 internal constant FAMILY_SHIFT = 27;
    uint32 internal constant LOCAL_MASK = 0x07FFFFFF; // low 27 bits
    uint32 internal constant FAMILY_MASK = 0x1F; // 5 bits

    error UnknownFamily(uint8 family);
    error LocalTooLarge(uint32 local);

    function isValidFamily(uint8 fam) internal pure returns (bool) {
        return fam >= FAMILY_SUI && fam <= FAMILY_APTOS;
    }

    function encode(uint8 fam, uint32 loc) internal pure returns (uint32) {
        if (!isValidFamily(fam)) revert UnknownFamily(fam);
        if (loc > LOCAL_MASK) revert LocalTooLarge(loc);
        return (uint32(fam) << FAMILY_SHIFT) | loc;
    }

    function family(uint32 internalId) internal pure returns (uint8) {
        return uint8((internalId >> FAMILY_SHIFT) & FAMILY_MASK);
    }

    function local(uint32 internalId) internal pure returns (uint32) {
        return internalId & LOCAL_MASK;
    }
}
