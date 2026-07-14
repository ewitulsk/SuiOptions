//! Base58 pubkey validation helpers.
//!
//! Solana ids are base58 pubkeys compared byte-exact as strings — the only
//! validation handlers need is "does this decode to 32 bytes", used to
//! reject garbage before an indexer round-trip (the Sui twin's
//! `ObjectId::from_hex` guard).

/// `Pubkey::default()` (32 zero bytes) in base58. The venue uses it as
/// "no bucket" on pure-swap auction events.
pub const ZERO_PUBKEY: &str = "11111111111111111111111111111111";

/// True iff `s` is a well-formed base58-encoded 32-byte pubkey.
pub fn is_pubkey(s: &str) -> bool {
    bs58::decode(s)
        .into_vec()
        .map(|v| v.len() == 32)
        .unwrap_or(false)
}

/// Map the zero pubkey (and empty strings, defensively) to `None`.
pub fn non_zero(s: &str) -> Option<&str> {
    if s.is_empty() || s == ZERO_PUBKEY {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_pubkeys() {
        assert!(is_pubkey("So11111111111111111111111111111111111111112"));
        assert!(is_pubkey(ZERO_PUBKEY));
        assert!(!is_pubkey("not-a-pubkey"));
        assert!(!is_pubkey("0x9c2b42a1")); // hex ids are the Sui world
        assert!(!is_pubkey("")); // empty
        // 16 bytes, valid base58 but not a pubkey.
        assert!(!is_pubkey(&bs58::encode([1u8; 16]).into_string()));
    }

    #[test]
    fn zero_pubkey_is_none() {
        assert_eq!(non_zero(ZERO_PUBKEY), None);
        assert_eq!(non_zero(""), None);
        assert_eq!(
            non_zero("So11111111111111111111111111111111111111112"),
            Some("So11111111111111111111111111111111111111112")
        );
    }

    #[test]
    fn zero_pubkey_constant_is_32_zero_bytes() {
        assert_eq!(bs58::decode(ZERO_PUBKEY).into_vec().unwrap(), vec![0u8; 32]);
    }
}
