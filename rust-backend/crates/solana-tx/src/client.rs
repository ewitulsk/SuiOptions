//! Solana RPC client + signer — the analog of sui-tx's `SuiClientWrapper`
//! and `submit_ptb`.

use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_response::RpcSimulateTransactionResult;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::transaction::Transaction;

use crate::network::Network;
use crate::signer::Signer;

/// Convenience wrapper: RpcClient + Signer + Network so callers pass one
/// thing around. Confirmed commitment everywhere — the UX tier the rest of
/// the stack reads at.
pub struct SolanaClientWrapper {
    pub client: RpcClient,
    pub signer: Signer,
    pub network: Network,
}

impl SolanaClientWrapper {
    pub fn connect(secrets: &runtime_config::Secrets, network: Network) -> Result<Self> {
        let signer = Signer::from_secrets(secrets, network).context("loading signer")?;
        // Prefer the operator's shared RPC override (a keyed Helius URL,
        // rendered into the [solana] block) over the public default.
        let rpc_url = secrets.resolve_solana_rpc_url(network.rpc_url());
        // Log the host only, never the full URL — the override carries a key.
        let rpc_host = rpc_url
            .split("://")
            .nth(1)
            .and_then(|s| s.split(['/', '?']).next())
            .unwrap_or("<unparseable>")
            .to_string();
        let client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        tracing::info!(%network, rpc_host, pubkey = %signer.pubkey(), "solana client ready");
        Ok(Self {
            client,
            signer,
            network,
        })
    }

    /// Fetch and Anchor-deserialize a program account (discriminator
    /// checked by `T::try_deserialize`).
    pub async fn get_account_deserialized<T: AccountDeserialize>(
        &self,
        pubkey: &Pubkey,
    ) -> Result<T> {
        let account = self
            .client
            .get_account(pubkey)
            .await
            .map_err(|e| anyhow!("fetching account {pubkey} failed: {e}"))?;
        T::try_deserialize(&mut account.data.as_slice())
            .map_err(|e| anyhow!("deserializing account {pubkey} failed: {e}"))
    }

    /// Sign `ixs` (fee payer = our signer, plus `extra_signers` such as
    /// fresh Position keypairs) against the latest blockhash.
    async fn signed_tx(
        &self,
        ixs: &[Instruction],
        extra_signers: &[&Keypair],
    ) -> Result<Transaction> {
        let blockhash = self
            .client
            .get_latest_blockhash()
            .await
            .context("fetching latest blockhash")?;
        let mut signers: Vec<&Keypair> = vec![&self.signer.keypair];
        signers.extend_from_slice(extra_signers);
        Ok(Transaction::new_signed_with_payer(
            ixs,
            Some(&self.signer.pubkey()),
            &signers,
            blockhash,
        ))
    }

    /// Simulate without submitting. The returned result carries the program
    /// logs — feed them to `errors::extract_error_code` on failure.
    pub async fn simulate(
        &self,
        ixs: &[Instruction],
        extra_signers: &[&Keypair],
        label: &str,
    ) -> Result<RpcSimulateTransactionResult> {
        let tx = self.signed_tx(ixs, extra_signers).await?;
        let resp = self
            .client
            .simulate_transaction(&tx)
            .await
            .map_err(|e| anyhow!("{label} simulation failed: {e}"))?;
        if let Some(err) = &resp.value.err {
            return Err(anyhow!(
                "{label} simulation failed: {err}; logs: {:?}",
                resp.value.logs
            ));
        }
        Ok(resp.value)
    }

    /// Sign, submit (preflight on — a failing tx surfaces its simulation
    /// logs in the error) and confirm at the client's confirmed commitment.
    pub async fn send_and_confirm(
        &self,
        ixs: &[Instruction],
        extra_signers: &[&Keypair],
        label: &str,
    ) -> Result<Signature> {
        let tx = self.signed_tx(ixs, extra_signers).await?;
        let signature = self
            .client
            .send_and_confirm_transaction(&tx)
            .await
            .map_err(|e| anyhow!("{label} failed: {e}"))?;
        tracing::debug!(%signature, label, "tx confirmed");
        Ok(signature)
    }
}
