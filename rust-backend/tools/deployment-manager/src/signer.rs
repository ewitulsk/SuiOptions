//! Deployer signing key.
//!
//! This used to be a local duplicate of `sui_tx::Signer`. Now that the
//! publish path submits through `sui_tx::tx::submit_ptb`, the two must be
//! the SAME type, so this is a re-export plus the network-aware loader the
//! duplicate carried (`sui_tx::Signer::from_secrets` takes `sui_tx::Network`,
//! this crate's CLI parses its own [`Network`]).

use anyhow::Result;

pub use sui_tx::Signer;

use crate::network::Network;

/// Load the signing keypair for `network` from the workspace secrets file.
/// There is no env-var fallback — every binary that signs reads its key from
/// the same TOML.
pub fn load_signer(secrets: &runtime_config::Secrets, network: Network) -> Result<Signer> {
    let raw = secrets.sui_private_key(network.as_str())?;
    Signer::from_string(raw.trim())
}
