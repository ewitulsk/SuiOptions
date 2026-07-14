//! Solana keypair loading — the analog of sui-tx's `Signer`.
//!
//! Keys come from the workspace secrets TOML (`[solana]` block, per-network
//! slots + `default` fallback; see `runtime_config::Secrets`). Two accepted
//! encodings, both of the 64-byte (secret ‖ public) keypair:
//!
//! - base58 (what Secrets Manager stores per the architecture doc), or
//! - the Solana-CLI JSON byte array (`[12, 34, …]`, e.g. `id.json`).

use anyhow::{anyhow, Context, Result};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer as _;

use crate::network::Network;

pub struct Signer {
    pub keypair: Keypair,
}

impl Signer {
    /// Load the keypair for `network` from the workspace secrets TOML.
    pub fn from_secrets(secrets: &runtime_config::Secrets, network: Network) -> Result<Self> {
        let raw = secrets.solana_keypair(network.as_str())?;
        Self::from_string(raw)
    }

    /// Parse a keypair from either encoding (see module docs).
    pub fn from_string(s: &str) -> Result<Self> {
        let s = s.trim();
        let bytes: Vec<u8> = if s.starts_with('[') {
            serde_json::from_str(s).context("parsing solana keypair JSON byte array")?
        } else {
            bs58::decode(s)
                .into_vec()
                .context("base58-decoding solana keypair")?
        };
        if bytes.len() != 64 {
            return Err(anyhow!(
                "solana keypair must be 64 bytes (secret ‖ public), got {}",
                bytes.len()
            ));
        }
        let keypair = Keypair::try_from(bytes.as_slice())
            .map_err(|e| anyhow!("invalid solana keypair bytes: {e}"))?;
        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base58_and_json_to_same_key() {
        let kp = Keypair::new();
        let bytes = kp.to_bytes();
        let b58 = bs58::encode(&bytes).into_string();
        let json = serde_json::to_string(&bytes.to_vec()).unwrap();

        let a = Signer::from_string(&b58).unwrap();
        let b = Signer::from_string(&json).unwrap();
        assert_eq!(a.pubkey(), kp.pubkey());
        assert_eq!(b.pubkey(), kp.pubkey());
    }

    #[test]
    fn rejects_wrong_length_and_garbage() {
        assert!(Signer::from_string("[1,2,3]").is_err());
        assert!(Signer::from_string("not-base58-!!!").is_err());
        // 32-byte base58 (a bare seed) is rejected — the accepted formats
        // are both 64-byte keypairs.
        let seed = bs58::encode(&[7u8; 32]).into_string();
        assert!(Signer::from_string(&seed).is_err());
    }
}
