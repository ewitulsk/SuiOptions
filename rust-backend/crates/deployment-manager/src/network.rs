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
