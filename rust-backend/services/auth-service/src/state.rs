//! Shared application state for both routers.

use crate::challenge::ChallengeStore;
use crate::db::repo::Repo;

pub struct AppState {
    /// HMAC secret for signing/verifying JWTs (from the secrets file).
    pub jwt_secret: String,
    /// Sui addresses auto-provisioned as admins on first wallet login (from
    /// config). This is the only path that creates an account without an
    /// invite — it exists so the first admin can get in at all.
    pub admin_addresses: Vec<String>,
    /// Live login challenges.
    pub challenges: ChallengeStore,
    /// Identity store.
    pub repo: Repo,
    /// Issued-token lifetime, seconds.
    pub token_ttl_secs: u64,
    /// Max age from issue a token may still be refreshed at, seconds.
    pub refresh_max_secs: u64,
    /// Default lifetime of a minted invite, seconds.
    pub invite_ttl_secs: i64,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        jwt_secret: String,
        admin_addresses: Vec<String>,
        repo: Repo,
        challenge_ttl_secs: u64,
        token_ttl_secs: u64,
        refresh_max_secs: u64,
        invite_ttl_secs: i64,
    ) -> Self {
        Self {
            jwt_secret,
            admin_addresses,
            challenges: ChallengeStore::new(challenge_ttl_secs),
            repo,
            token_ttl_secs,
            refresh_max_secs,
            invite_ttl_secs,
        }
    }
}
