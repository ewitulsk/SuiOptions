/// Internal chain-id encoding (bridge-spec.md §2.2 — "internal registry ID,
/// NOT the native chainid").
///
/// The 32-bit internal id is self-describing: the top 5 bits are the chain
/// *family*, the low 27 bits are a per-family local id.
///
/// ```
///  31      27 26                              0
/// ┌──────────┬─────────────────────────────────┐
/// │  family  │            local id             │
/// │  5 bits  │            27 bits              │
/// └──────────┴─────────────────────────────────┘
///
/// internal_id = (family << 27) | local
/// ```
///
/// Families: 1 = Sui, 2 = EVM, 3 = Solana, 4 = Aptos. Recovering the family is
/// a shift (`family(id)`), so no registry lookup or separately-stored field is
/// needed and the two can never disagree.
///
/// The 27-bit local field caps at 134,217,727. For EVM it SHOULD be the native
/// chainId when it fits (HyperEVM testnet = 998 does); chains whose chainId
/// exceeds the ceiling must use an assigned index instead, with the registry's
/// `native_identifier` always holding the authoritative value.
module sui_bridge::chain_id;

use sui_bridge::errors;

const FAMILY_SHIFT: u8 = 27;
const FAMILY_MASK: u32 = 0x1F; // 5 bits
const LOCAL_MASK: u32 = 0x07FF_FFFF; // low 27 bits

const FAMILY_SUI: u8 = 1;
const FAMILY_EVM: u8 = 2;
const FAMILY_SOLANA: u8 = 3;
const FAMILY_APTOS: u8 = 4;

public fun family_sui(): u8 { FAMILY_SUI }
public fun family_evm(): u8 { FAMILY_EVM }
public fun family_solana(): u8 { FAMILY_SOLANA }
public fun family_aptos(): u8 { FAMILY_APTOS }

public fun local_id_ceiling(): u32 { LOCAL_MASK }

public fun is_valid_family(f: u8): bool { f >= FAMILY_SUI && f <= FAMILY_APTOS }

/// Compose an internal id from `family` and a 27-bit `local` id.
public fun new(family: u8, local: u32): u32 {
    assert!(is_valid_family(family), errors::unknown_family());
    assert!(local <= LOCAL_MASK, errors::chain_local_too_large());
    ((family as u32) << FAMILY_SHIFT) | local
}

/// The family tag (top 5 bits).
public fun family(internal_id: u32): u8 {
    (((internal_id >> FAMILY_SHIFT) & FAMILY_MASK) as u8)
}

/// The per-family local id (low 27 bits).
public fun local(internal_id: u32): u32 {
    internal_id & LOCAL_MASK
}
