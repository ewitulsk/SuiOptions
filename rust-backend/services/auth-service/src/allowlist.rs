//! Admin allowlist helpers.
//!
//! The set of permitted addresses is config-driven (`admin_addresses` in the
//! service config); this module only normalizes and matches against it.
//! Membership is the *only* authorization check — there are no roles or scopes.

/// Canonical form for comparison: lowercase, `0x`-prefixed, 32-byte
/// (64-hex-char) zero-padded. Mirrors how Sui renders addresses.
pub fn normalize(addr: &str) -> String {
    let s = addr.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
        .to_ascii_lowercase();
    format!("0x{:0>64}", hex)
}

/// Whether `addr` is on `allowlist` (both sides normalized).
pub fn is_allowed(allowlist: &[String], addr: &str) -> bool {
    let want = normalize(addr);
    allowlist.iter().any(|a| normalize(a) == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pads_and_lowercases() {
        assert_eq!(normalize("0xAB"), format!("0x{:0>64}", "ab"));
        assert_eq!(normalize("ab"), format!("0x{:0>64}", "ab"));
    }

    #[test]
    fn matches_regardless_of_case_and_padding() {
        let list = vec![
            "0xAB8D1B5A5311C9400E3EAF5C3B641F10FB48B43CC30D365FA8A98A6CA6BD4865".to_string(),
        ];
        let lower = "0xab8d1b5a5311c9400e3eaf5c3b641f10fb48b43cc30d365fa8a98a6ca6bd4865";
        assert!(is_allowed(&list, lower));
        assert!(is_allowed(&list, &lower.to_ascii_uppercase()));
    }

    #[test]
    fn unknown_address_rejected() {
        let list = vec!["0xab".to_string()];
        assert!(!is_allowed(&list, "0x01"));
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        assert!(!is_allowed(&[], "0xab"));
    }
}
