//! Sui RPC client + signer.
//!
//! Wraps `sui_sdk::SuiClient` for RPC, and a `Signer` loaded from env vars
//! (`SUI_PRIVATE_KEY_TESTNET`, etc., with `SUI_PRIVATE_KEY` as a fallback).
//! Identical pattern to `deployment-manager::signer` so the same env vars
//! work across every binary in the workspace.

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sui_sdk::{SuiClient, SuiClientBuilder};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Devnet,
}

impl Network {
    pub fn rpc_url(self) -> &'static str {
        match self {
            Self::Mainnet => sui_sdk::SUI_MAINNET_URL,
            Self::Testnet => sui_sdk::SUI_TESTNET_URL,
            Self::Devnet => sui_sdk::SUI_DEVNET_URL,
        }
    }
    pub fn priv_key_env(self) -> &'static str {
        match self {
            Self::Mainnet => "SUI_PRIVATE_KEY_MAINNET",
            Self::Testnet => "SUI_PRIVATE_KEY_TESTNET",
            Self::Devnet => "SUI_PRIVATE_KEY_DEVNET",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Devnet => "devnet",
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub struct Signer {
    pub keypair: SuiKeyPair,
    pub address: SuiAddress,
}

impl Signer {
    /// Loads the keypair from an env var. Prefers per-network
    /// (`SUI_PRIVATE_KEY_TESTNET`), falls back to the generic `SUI_PRIVATE_KEY`
    /// so single-key setups still work.
    pub fn from_env(network: Network) -> Result<Self> {
        let specific = network.priv_key_env();
        let raw = std::env::var(specific)
            .or_else(|_| std::env::var("SUI_PRIVATE_KEY"))
            .with_context(|| format!("neither {specific} nor SUI_PRIVATE_KEY is set"))?;
        Self::from_string(raw.trim())
    }

    pub fn from_string(s: &str) -> Result<Self> {
        let keypair =
            SuiKeyPair::decode(s).map_err(|e| anyhow!("decoding SUI private key: {e}"))?;
        let address = SuiAddress::from(&keypair.public());
        Ok(Self { keypair, address })
    }
}

/// Convenience wrapper: SuiClient + Signer + Network so callers pass one
/// thing around.
pub struct SuiClientWrapper {
    pub client: SuiClient,
    pub signer: Signer,
    pub network: Network,
}

impl SuiClientWrapper {
    pub async fn connect(network: Network) -> Result<Self> {
        let signer = Signer::from_env(network).context("loading signer")?;
        let client = SuiClientBuilder::default()
            .build(network.rpc_url())
            .await
            .with_context(|| format!("building SuiClient for {network}"))?;
        tracing::info!(%network, address = %signer.address, "sui client ready");
        Ok(Self {
            client,
            signer,
            network,
        })
    }
}
