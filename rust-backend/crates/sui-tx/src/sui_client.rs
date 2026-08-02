//! Sui chain client + signer.
//!
//! Wraps the gRPC [`ChainClient`] and the GraphQL [`EventClient`] for chain
//! access, plus a `Signer` loaded from the workspace secrets file (see
//! `crates/secrets`). No env-var fallback — every binary reads its key from
//! the same TOML.
//!
//! JSON-RPC (`sui_sdk::SuiClient`) is gone: Sui deactivated it on fullnodes.
//! See `docs/sui-json-rpc-migration.md`.

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;

use crate::chain::ChainClient;
use crate::events::EventClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Devnet,
}

impl Network {
    /// Public gRPC endpoint. Unlike the retired JSON-RPC default, this one
    /// is live — the fullnodes serve gRPC on the same host/port.
    pub fn grpc_url(self) -> &'static str {
        match self {
            Self::Mainnet => "https://fullnode.mainnet.sui.io:443",
            Self::Testnet => "https://fullnode.testnet.sui.io:443",
            Self::Devnet => "https://fullnode.devnet.sui.io:443",
        }
    }

    /// Public GraphQL endpoint, used for event reads only.
    pub fn graphql_url(self) -> &'static str {
        match self {
            Self::Mainnet => "https://graphql.mainnet.sui.io/graphql",
            Self::Testnet => "https://graphql.testnet.sui.io/graphql",
            Self::Devnet => "https://graphql.devnet.sui.io/graphql",
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
    pub fn from_secrets(secrets: &runtime_config::Secrets, network: Network) -> Result<Self> {
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

/// Convenience wrapper: chain client + event client + signer + network so
/// callers pass one thing around.
pub struct SuiClientWrapper {
    /// gRPC reads and writes.
    pub client: ChainClient,
    /// GraphQL event reads (gRPC has no events query).
    pub events: EventClient,
    pub signer: Signer,
    pub network: Network,
}

impl SuiClientWrapper {
    pub async fn connect(secrets: &runtime_config::Secrets, network: Network) -> Result<Self> {
        let signer = Signer::from_secrets(secrets, network).context("loading signer")?;
        // Prefer the operator's shared endpoint overrides (rendered into the
        // [sui] block of the service's secrets toml) over the public
        // defaults.
        let grpc_url = secrets.resolve_grpc_url(network.grpc_url());
        let graphql_url = secrets.resolve_graphql_url(network.graphql_url());
        let client = ChainClient::new(&grpc_url)
            .with_context(|| format!("building chain client for {network}"))?;
        let events = EventClient::new(&graphql_url);
        // Log the host only, never the full URL — an override can carry a
        // token in its path.
        tracing::info!(
            %network,
            grpc_host = client.host(),
            address = %signer.address,
            "sui client ready"
        );
        Ok(Self {
            client,
            events,
            signer,
            network,
        })
    }
}
