use anyhow::{anyhow, Context, Result};
use sui_types::base_types::SuiAddress;
use sui_types::crypto::SuiKeyPair;

use crate::network::Network;

pub struct Signer {
    pub keypair: SuiKeyPair,
    pub address: SuiAddress,
}

impl Signer {
    /// Loads the signing keypair for `network`. Prefers the per-network env
    /// var (e.g. `SUI_PRIVATE_KEY_TESTNET`), falling back to the shared
    /// `SUI_PRIVATE_KEY` so single-key setups still work.
    pub fn load(network: Network) -> Result<Self> {
        let specific = network.priv_key_env();
        let raw = std::env::var(specific)
            .or_else(|_| std::env::var("SUI_PRIVATE_KEY"))
            .with_context(|| {
                format!("neither {specific} nor SUI_PRIVATE_KEY is set")
            })?;

        let keypair = SuiKeyPair::decode(raw.trim())
            .map_err(|e| anyhow!("failed to decode SUI private key: {e}"))?;
        let address = SuiAddress::from(&keypair.public());

        Ok(Self { keypair, address })
    }
}
