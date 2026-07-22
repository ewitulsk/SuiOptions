//! Faucet-backed wallet funding.
//!
//! The sim's only liquidity source is the deployment's test-token faucets
//! (this service is testnet-only by construction), so unlike mm-bot's
//! pluggable `LiquiditySource` there is no trait here — just the faucet
//! minter. Best-effort by contract: returns the realized balance (which
//! may be below `target` if a faucet is missing or wedged) instead of
//! erroring, so a banding pass degrades rather than aborts.

use std::collections::HashMap;

use sui_sdk::SuiClient;

use protocol_types::asset::canonicalize_move_type;
use sui_tx::sui_client::Signer;
use sui_tx::tx::test_tokens::mint_to_sender;
use token_info_client::{TestTokens, TokenInfo};

pub struct FaucetMinter {
    /// canonical coin type → its faucet record.
    faucets: HashMap<String, TokenInfo>,
    gas_budget: u64,
}

impl FaucetMinter {
    /// Build from the token-info snapshot's testTokens block.
    pub fn new(test_tokens: Option<&TestTokens>, gas_budget: u64) -> Self {
        let faucets = test_tokens
            .map(|tt| {
                tt.tokens
                    .values()
                    .map(|info| (canonicalize_move_type(&info.coin_type), info.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Self { faucets, gas_budget }
    }

    /// Ensure at least `target` atomic units of `coin_type` are spendable in
    /// the signer's wallet, minting the shortfall from the faucet if short.
    /// Returns the now-available wallet balance.
    pub async fn ensure_wallet_balance(
        &self,
        client: &SuiClient,
        signer: &Signer,
        coin_type: &str,
        target: u64,
    ) -> u64 {
        let have = wallet_balance(client, signer, coin_type).await;
        if have >= target {
            return have;
        }
        let Some(faucet) = self.faucets.get(&canonicalize_move_type(coin_type)) else {
            return have; // no faucet for this coin — supply what we have
        };
        let shortfall = target - have;
        let (tokens_pkg, module) = match faucet.module_path() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: bad faucet record; skipping top-up");
                return have;
            }
        };
        let faucet_id = match faucet.faucet() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: bad faucet id; skipping top-up");
                return have;
            }
        };
        match mint_to_sender(client, signer, tokens_pkg, &module, faucet_id, shortfall, self.gas_budget).await {
            Ok(resp) => {
                tracing::info!(coin_type, shortfall, digest = %resp.digest, "liquidity: minted wallet top-up");
                wallet_balance(client, signer, coin_type).await
            }
            // A wedged faucet must not kill the banding cycle: log and proceed
            // with what we have; the next cycle retries.
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: wallet top-up mint failed");
                have
            }
        }
    }
}

/// Current wallet balance of `coin_type`, clamped to `u64`. `0` on RPC error
/// (the caller's downstream sizing already tolerates an under-read).
pub async fn wallet_balance(client: &SuiClient, signer: &Signer, coin_type: &str) -> u64 {
    client
        .coin_read_api()
        .get_balance(signer.address, Some(coin_type.to_string()))
        .await
        .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
