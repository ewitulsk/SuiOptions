use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Devnet,
}

impl Network {
    pub const ALL: [Network; 3] = [Network::Mainnet, Network::Testnet, Network::Devnet];

    /// Public gRPC endpoint for this network. JSON-RPC is deactivated on
    /// Sui fullnodes; gRPC is served on the same host/port.
    pub fn grpc_url(self) -> &'static str {
        match self {
            Self::Mainnet => sui_tx::Network::Mainnet.grpc_url(),
            Self::Testnet => sui_tx::Network::Testnet.grpc_url(),
            Self::Devnet => sui_tx::Network::Devnet.grpc_url(),
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
