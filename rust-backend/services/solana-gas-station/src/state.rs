//! Shared application state.

use std::collections::HashMap;

use solana_tx::SolanaClientWrapper;

use crate::faucet::FaucetToken;
use crate::sponsor::SponsorPolicy;
use crate::template::TxTemplate;

pub struct AppState {
    /// Solana RPC client + station signer (the fee payer / mint authority).
    pub solana: SolanaClientWrapper,
    /// The exact transaction shapes the station will sponsor. Built at
    /// boot from the solana-token-info snapshot; a transaction matching
    /// none is refused.
    pub templates: Vec<TxTemplate>,
    /// Lamport-delta cap + balance health threshold.
    pub policy: SponsorPolicy,
    /// Whether `/faucet` is live (config flag AND non-mainnet network).
    pub faucet_enabled: bool,
    /// Mintable test tokens, keyed by UPPER ticker. Empty when the faucet
    /// is disabled.
    pub faucet_tokens: HashMap<String, FaucetToken>,
}
