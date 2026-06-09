//! Shared application state.

use sui_tx::tx::sponsor::BudgetPolicy;
use sui_tx::tx::template::PtbTemplate;
use sui_tx::SuiClientWrapper;

pub struct AppState {
    /// Sui RPC client + sponsor signer (the gas payer).
    pub sui: SuiClientWrapper,
    /// The exact PTB shapes the station will sponsor (the frontend's
    /// write/buy/exercise/redeem/faucet flows). Built at boot from the
    /// token-info snapshot; a PTB matching none is refused.
    pub templates: Vec<PtbTemplate>,
    /// Gas-budget sizing policy.
    pub policy: BudgetPolicy,
    /// Balance (MIST) below which `/balance` reports unhealthy.
    pub min_balance_threshold_mist: u64,
}
