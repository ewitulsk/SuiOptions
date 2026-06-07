//! Shared application state for both routers.

use crate::challenge::ChallengeStore;

pub struct AppState {
    /// HMAC secret for signing/verifying JWTs (from the secrets file).
    pub jwt_secret: String,
    /// Sui addresses permitted to obtain a JWT (from config).
    pub admin_addresses: Vec<String>,
    /// Live login challenges.
    pub challenges: ChallengeStore,
    /// Issued-token lifetime, seconds.
    pub token_ttl_secs: u64,
    /// Max age from issue a token may still be refreshed at, seconds.
    pub refresh_max_secs: u64,
}

impl AppState {
    pub fn new(
        jwt_secret: String,
        admin_addresses: Vec<String>,
        challenge_ttl_secs: u64,
        token_ttl_secs: u64,
        refresh_max_secs: u64,
    ) -> Self {
        Self {
            jwt_secret,
            admin_addresses,
            challenges: ChallengeStore::new(challenge_ttl_secs),
            token_ttl_secs,
            refresh_max_secs,
        }
    }
}
