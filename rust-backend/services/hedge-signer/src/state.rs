//! Shared application state.

use std::collections::HashMap;
use std::sync::Arc;

use sui_tx::SuiClientWrapper;

use crate::audit::AuditLog;
use crate::chain::VaultResolver;
use crate::frost::Ceremonies;
use crate::policy::VaultPolicy;

pub struct AppState {
    /// Sui RPC client + the service's multisig member key (the co-signer).
    pub sui: SuiClientWrapper,
    /// Per-vault policy, keyed by `vault_id`. Built at boot from config +
    /// the token-info snapshot; a /sign for an unknown vault is refused.
    pub vaults: HashMap<String, VaultPolicy>,
    /// Append-only JSONL decision log. Shared with [`FrostState`] so both
    /// signing paths append to the one stream.
    pub audit: Arc<AuditLog>,
}

/// State for the `/frost/*` surface — deliberately separate from
/// [`AppState`]: the FROST path needs no signing key, and its one chain
/// dependency is behind [`VaultResolver`], which keeps it constructible in
/// tests without network access.
pub struct FrostState {
    /// Same per-vault policies as [`AppState::vaults`] (cloned at boot).
    /// Gates SIGNING only — keygen is open to any live vault.
    pub vaults: HashMap<String, VaultPolicy>,
    /// Shared decision log.
    pub audit: Arc<AuditLog>,
    /// Share store + in-flight keygen/signing ceremonies.
    pub ceremonies: Ceremonies,
    /// On-chain vault validation behind the keygen gate.
    pub chain: Arc<dyn VaultResolver>,
}
