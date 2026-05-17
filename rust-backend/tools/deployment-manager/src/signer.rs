use anyhow::{anyhow, Result};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;

use crate::network::Network;

pub struct Signer {
    pub keypair: SuiKeyPair,
    pub address: SuiAddress,
}

impl Signer {
    /// Load the signing keypair for `network` from the workspace secrets
    /// file. There is no env-var fallback — every binary that signs reads
    /// its key from the same TOML.
    pub fn from_secrets(secrets: &shared::Secrets, network: Network) -> Result<Self> {
        let raw = secrets.sui_private_key(network.as_str())?;
        Self::from_string(raw.trim())
    }

    pub fn from_string(s: &str) -> Result<Self> {
        let keypair = SuiKeyPair::decode(s)
            .map_err(|e| anyhow!("failed to decode SUI private key: {e}"))?;
        let address = SuiAddress::from(&keypair.public());
        Ok(Self { keypair, address })
    }
}
