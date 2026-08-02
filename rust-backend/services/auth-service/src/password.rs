//! Password identities: username rules and Argon2id hashing.
//!
//! We store no email, so there is no "reset via link" path — a forgotten
//! password means an admin mints a fresh invite. That makes the username rules
//! below load-bearing for support: they have to be unambiguous to read back
//! over a channel a human is using.

use anyhow::{bail, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

/// Shortest password we accept. Deliberately a length floor rather than a
/// character-class rule: composition rules push users toward `Passw0rd!` and
/// buy nothing.
pub const MIN_PASSWORD_LEN: usize = 12;
/// Argon2 hashes the input, so length is otherwise unbounded — this only stops
/// a megabyte body burning CPU.
pub const MAX_PASSWORD_LEN: usize = 1024;

const MIN_USERNAME_LEN: usize = 3;
const MAX_USERNAME_LEN: usize = 64;

/// Normalize a username to its stored form: trimmed and lowercased.
///
/// Lowercasing is what makes the `UNIQUE (kind, identifier)` index
/// case-insensitive, so `Evan` cannot be registered alongside `evan` and
/// impersonate it.
pub fn normalize_username(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Validate an already-normalized username.
///
/// ASCII alphanumerics plus `.`, `-` and `_`, and it must start with a letter
/// or digit. Rejecting everything else keeps usernames free of the whitespace
/// and lookalike Unicode that make two accounts indistinguishable on screen.
pub fn validate_username(name: &str) -> Result<()> {
    if name.len() < MIN_USERNAME_LEN || name.len() > MAX_USERNAME_LEN {
        bail!("username must be {MIN_USERNAME_LEN}-{MAX_USERNAME_LEN} characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("username may contain only letters, digits, '.', '-' and '_'");
    }
    if !name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        bail!("username must start with a letter or digit");
    }
    Ok(())
}

pub fn validate_password(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_LEN {
        bail!("password must be at least {MIN_PASSWORD_LEN} characters");
    }
    if password.len() > MAX_PASSWORD_LEN {
        bail!("password must be at most {MAX_PASSWORD_LEN} characters");
    }
    Ok(())
}

/// Hash to a PHC string (`$argon2id$v=19$m=...`). The parameters travel inside
/// the string, so raising them later still verifies today's hashes.
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
}

/// Constant-time verify. A malformed stored hash verifies as `false` rather
/// than erroring, so a corrupt row cannot be told apart from a wrong password.
pub fn verify(password: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_then_verify() {
        let h = hash("correct horse battery").unwrap();
        assert!(verify("correct horse battery", &h));
        assert!(!verify("wrong horse battery", &h));
    }

    #[test]
    fn hashes_are_salted() {
        // Same password, different salt each time — identical hashes would leak
        // which accounts share a password.
        assert_ne!(hash("correct horse battery").unwrap(), hash("correct horse battery").unwrap());
    }

    #[test]
    fn malformed_hash_verifies_false() {
        assert!(!verify("anything", "not-a-phc-string"));
        assert!(!verify("anything", ""));
    }

    #[test]
    fn username_normalization_is_case_folding() {
        assert_eq!(normalize_username("  EvanW  "), "evanw");
    }

    #[test]
    fn username_rules() {
        assert!(validate_username("evan").is_ok());
        assert!(validate_username("evan.w-1_2").is_ok());
        assert!(validate_username("ev").is_err(), "too short");
        assert!(validate_username("_evan").is_err(), "must start alphanumeric");
        assert!(validate_username("evan w").is_err(), "no whitespace");
        assert!(validate_username("evan@example.com").is_err(), "no '@' — not an email store");
        assert!(validate_username(&"a".repeat(65)).is_err(), "too long");
    }

    #[test]
    fn password_length_bounds() {
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_LEN)).is_ok());
        assert!(validate_password(&"a".repeat(MIN_PASSWORD_LEN - 1)).is_err());
        assert!(validate_password(&"a".repeat(MAX_PASSWORD_LEN + 1)).is_err());
    }
}
