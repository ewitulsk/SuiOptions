//! Shared application state.

use std::collections::HashMap;
use std::sync::Arc;

use sui_tx::SuiClientWrapper;

use crate::audit::AuditLog;
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
/// [`AppState`]: the FROST path needs no Sui RPC client, which keeps it
/// constructible in tests without network access.
pub struct FrostState {
    /// Same per-vault policies as [`AppState::vaults`] (cloned at boot).
    pub vaults: HashMap<String, VaultPolicy>,
    /// Shared decision log.
    pub audit: Arc<AuditLog>,
    /// Share store + in-flight keygen/signing ceremonies.
    pub ceremonies: Ceremonies,
}
