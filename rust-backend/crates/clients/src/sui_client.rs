//! Sui RPC client + signer.
//!
//! Wraps `sui_sdk::SuiClient` for RPC, and a `Signer` loaded from the
//! workspace secrets file (see `crates/secrets`). No env-var fallback —
//! every binary reads its key from the same TOML.

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
    /// Load the keypair from the workspace secrets TOML.
    pub fn from_secrets(secrets: &secrets::Secrets, network: Network) -> Result<Self> {
        let raw = secrets.sui_private_key(network.as_str())?;
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
    pub async fn connect(secrets: &secrets::Secrets, network: Network) -> Result<Self> {
        let signer = Signer::from_secrets(secrets, network).context("loading signer")?;
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
