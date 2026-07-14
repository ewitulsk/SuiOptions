//! Admin allowlist helpers.
//!
//! The set of permitted addresses is config-driven (`admin_addresses` in the
//! service config); this module only matches against it. Membership is the
//! *only* authorization check — there are no roles or scopes.
//!
//! Unlike the Sui twin there is no normalization: a Solana address IS the
//! base58 encoding of the ed25519 pubkey, base58 is case-sensitive, and there
//! is exactly one canonical rendering. Comparison is exact string match.

/// Whether `addr` is on `allowlist` (exact string match).
pub fn is_allowed(allowlist: &[String], addr: &str) -> bool {
    allowlist.iter().any(|a| a == addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_allowed() {
        let list = vec!["4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T".to_string()];
        assert!(is_allowed(&list, "4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T"));
    }

    #[test]
    fn case_differences_rejected() {
        // base58 is case-sensitive; a different casing is a different address.
        let list = vec!["4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T".to_string()];
        assert!(!is_allowed(&list, "4nd1mbqtrmjvyvfkf2pjy9nzuzdtasp7d4xwls4gdb4t"));
    }

    #[test]
    fn unknown_address_rejected() {
        let list = vec!["4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T".to_string()];
        assert!(!is_allowed(&list, "11111111111111111111111111111111"));
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        assert!(!is_allowed(&[], "4Nd1mBQtrMJVYVfKf2PJy9NZUZdTAsp7D4xWLs4gDB4T"));
    }
}
