//! Shared application state.

use std::collections::HashMap;

use sui_tx::SuiClientWrapper;

use crate::audit::AuditLog;
use crate::policy::VaultPolicy;

pub struct AppState {
    /// Sui RPC client + the service's multisig member key (the co-signer).
    pub sui: SuiClientWrapper,
    /// Per-vault policy, keyed by `vault_id`. Built at boot from config +
    /// the token-info snapshot; a /sign for an unknown vault is refused.
    pub vaults: HashMap<String, VaultPolicy>,
    /// Append-only JSONL decision log.
    pub audit: AuditLog,
}
