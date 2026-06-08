//! Shared application state.

use sui_tx::tx::sponsor::BudgetPolicy;
use sui_tx::SuiClientWrapper;
use sui_types::base_types::ObjectID;

pub struct AppState {
    /// Sui RPC client + sponsor signer (the gas payer).
    pub sui: SuiClientWrapper,
    /// Packages the station will sponsor. Empty = allow all.
    pub allowed_packages: Vec<ObjectID>,
    /// Gas-budget sizing policy.
    pub policy: BudgetPolicy,
    /// Balance (MIST) below which `/balance` reports unhealthy.
    pub min_balance_threshold_mist: u64,
}
