//! Pluggable trading-liquidity source.
//!
//! A real market maker funds its bots from somewhere — a treasury, a bridge,
//! a CEX withdrawal, an on-chain swap. The bot shouldn't care which: before it
//! quotes, it asks its [`LiquiditySource`] to make sure it holds enough of each
//! coin it's about to commit, and the source pulls more if short.
//!
//! On testnet/staging the default [`FaucetLiquiditySource`] mints test tokens
//! from the deployment's faucets. To run this bot against real liquidity, a
//! market maker implements [`LiquiditySource`] against their own funding path
//! and swaps it in at the one construction site in `main.rs` — no other code
//! changes.

use std::collections::HashMap;

use async_trait::async_trait;
use sui_sdk::SuiClient;
use sui_types::base_types::ObjectID;

use protocol_types::asset::canonicalize_move_type;
use sui_tx::sui_client::Signer;
use sui_tx::tx::test_tokens::{mint_and_deposit_into_account, mint_to_sender};
use token_info_client::{TestTokens, TokenInfo};

/// A source the bot pulls trading liquidity from. Implement this to fund the
/// bot from your own treasury/bridge/exchange instead of the test faucets.
///
/// Both methods are **best-effort**: they return the realized balance (which
/// may be below `target` if the source is exhausted) and MUST NOT error just
/// because they can't supply a particular coin — return the current balance
/// instead, so the quoting loop degrades to whatever real funds exist rather
/// than aborting.
#[async_trait]
pub trait LiquiditySource: Send + Sync {
    /// Ensure at least `target` atomic units of `coin_type` are spendable in
    /// the signer's wallet, pulling from the source if short. Returns the
    /// now-available wallet balance.
    async fn ensure_wallet_balance(
        &self,
        client: &SuiClient,
        signer: &Signer,
        coin_type: &str,
        target: u64,
    ) -> u64;

    /// Optional: deposit `amount` atomic units of `coin_type` into the
    /// protocol trading `Account` (the writer-MM funds option writes from
    /// there). Defaults to a no-op (returns `false`): an external source funds
    /// the wallet; the bot can move wallet→Account itself. The faucet impl
    /// overrides this. Returns whether a deposit happened.
    async fn ensure_account_balance(
        &self,
        _client: &SuiClient,
        _signer: &Signer,
        _account_id: ObjectID,
        _coin_type: &str,
        _amount: u64,
    ) -> bool {
        false
    }
}

/// Default source: mint the shortfall from the deployment's test-token
/// faucets. A no-op for any coin without a faucet (and for every coin on
/// mainnet, where there are no test tokens) — exactly the "can't supply this
/// coin" contract.
pub struct FaucetLiquiditySource {
    /// canonical coin type → its faucet record.
    faucets: HashMap<String, TokenInfo>,
    /// Options protocol package, for the `account::deposit` half of the
    /// Account top-up.
    options_package: ObjectID,
    gas_budget: u64,
}

impl FaucetLiquiditySource {
    /// Build from the token-info snapshot's testTokens block (`None` on
    /// mainnet → a source that no-ops for everything).
    pub fn new(test_tokens: Option<&TestTokens>, options_package: ObjectID, gas_budget: u64) -> Self {
        let faucets = test_tokens
            .map(|tt| {
                tt.tokens
                    .values()
                    .map(|info| (canonicalize_move_type(&info.coin_type), info.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Self { faucets, options_package, gas_budget }
    }

    fn faucet_for(&self, coin_type: &str) -> Option<&TokenInfo> {
        self.faucets.get(&canonicalize_move_type(coin_type))
    }
}

/// Current wallet balance of `coin_type`, clamped to `u64`. `0` on RPC error
/// (the caller's downstream sizing already tolerates an under-read).
async fn wallet_balance(client: &SuiClient, signer: &Signer, coin_type: &str) -> u64 {
    client
        .coin_read_api()
        .get_balance(signer.address, Some(coin_type.to_string()))
        .await
        .map(|bal| bal.total_balance.min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[async_trait]
impl LiquiditySource for FaucetLiquiditySource {
    async fn ensure_wallet_balance(
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
        let Some(faucet) = self.faucet_for(coin_type) else {
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
            // A wedged faucet must not kill the quote cycle: log and proceed
            // with what we have; the next cycle retries.
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: wallet top-up mint failed");
                have
            }
        }
    }

    async fn ensure_account_balance(
        &self,
        client: &SuiClient,
        signer: &Signer,
        account_id: ObjectID,
        coin_type: &str,
        amount: u64,
    ) -> bool {
        let Some(faucet) = self.faucet_for(coin_type) else {
            return false;
        };
        let (tokens_pkg, module) = match faucet.module_path() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: bad faucet record; skipping account top-up");
                return false;
            }
        };
        let faucet_id = match faucet.faucet() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: bad faucet id; skipping account top-up");
                return false;
            }
        };
        match mint_and_deposit_into_account(
            client,
            signer,
            tokens_pkg,
            &module,
            faucet_id,
            coin_type,
            account_id,
            self.options_package,
            amount,
            self.gas_budget,
        )
        .await
        {
            Ok(resp) => {
                tracing::info!(coin_type, amount, digest = %resp.digest, "liquidity: minted account top-up");
                true
            }
            Err(e) => {
                tracing::warn!(coin_type, error = %format!("{e:#}"), "liquidity: account top-up mint failed");
                false
            }
        }
    }
}
