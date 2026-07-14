//! Solana cluster selection — the analog of sui-tx's `Network`.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum Network {
    Devnet,
    Testnet,
    /// Accepts both `mainnet` and `mainnet-beta` when parsed.
    #[serde(alias = "mainnet")]
    #[clap(alias = "mainnet")]
    MainnetBeta,
}

impl Network {
    /// Public cluster default; overridden per-operator via the shared
    /// `solana.rpc_url` secret (see `SolanaClientWrapper::connect`).
    pub fn rpc_url(self) -> &'static str {
        match self {
            Self::Devnet => "https://api.devnet.solana.com",
            Self::Testnet => "https://api.testnet.solana.com",
            Self::MainnetBeta => "https://api.mainnet-beta.solana.com",
        }
    }

    /// Canonical name — also the slot key `Secrets::solana_keypair` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Devnet => "devnet",
            Self::Testnet => "testnet",
            Self::MainnetBeta => "mainnet-beta",
        }
    }
}

impl std::str::FromStr for Network {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "devnet" => Ok(Self::Devnet),
            "testnet" => Ok(Self::Testnet),
            "mainnet" | "mainnet-beta" => Ok(Self::MainnetBeta),
            other => Err(anyhow::anyhow!("unknown solana network: {other}")),
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_aliases() {
        assert_eq!("devnet".parse::<Network>().unwrap(), Network::Devnet);
        assert_eq!("Testnet".parse::<Network>().unwrap(), Network::Testnet);
        assert_eq!("mainnet".parse::<Network>().unwrap(), Network::MainnetBeta);
        assert_eq!(
            "mainnet-beta".parse::<Network>().unwrap(),
            Network::MainnetBeta
        );
        assert!("localnet".parse::<Network>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for n in [Network::Devnet, Network::Testnet, Network::MainnetBeta] {
            assert_eq!(n.as_str().parse::<Network>().unwrap(), n);
        }
    }
}
