//! Shared application state.

use ed25519_dalek::VerifyingKey;

use crate::config::Config;
use crate::dakota::DakotaClient;
use crate::db::repo::Repo;
use crate::invites::InviteClient;
use crate::wallet::WalletSigner;

pub struct AppState {
    pub cfg: Config,
    pub repo: Repo,
    pub dakota: DakotaClient,
    /// Verifies webhook deliveries. Parsed once at boot so a malformed key is
    /// a startup failure rather than a silent flood of rejected deliveries.
    pub webhook_key: VerifyingKey,
    pub invites: InviteClient,
    /// Treasury signing key. `None` when `dakota.wallet_p256_pem` is unset —
    /// every other feature works without it, so a missing key degrades the
    /// treasury rather than blocking startup.
    pub wallet_signer: Option<WalletSigner>,
}

impl AppState {
    pub fn new(
        cfg: Config,
        repo: Repo,
        dakota: DakotaClient,
        webhook_key: VerifyingKey,
        invites: InviteClient,
        wallet_signer: Option<WalletSigner>,
    ) -> Self {
        Self { cfg, repo, dakota, webhook_key, invites, wallet_signer }
    }
}
