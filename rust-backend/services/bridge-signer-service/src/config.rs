use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use runtime_config::config_load;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Public API bind (spec §5.3 port 3000): `/sign_message`, `/health`,
    /// `/get_attestation`, `/group_keys`.
    pub public_bind_addr: SocketAddr,
    /// Admin API bind (spec §5.3 port 3001) — localhost only in production.
    pub admin_bind_addr: SocketAddr,

    /// M1-ONLY: 32-byte hex seed for the single-party Ed25519 key (Sui group
    /// key). At M3 the share is Seal-provisioned in-enclave instead and never
    /// touches config. Supports `${ENV}` expansion.
    pub ed25519_seed_hex: String,
    /// M1-ONLY: 32-byte hex seed for the single-party secp256k1 key (EVM group
    /// key). Must be a valid non-zero scalar below the curve order.
    pub secp256k1_seed_hex: String,

    /// Registered group-key ids the envelope references, selected by the
    /// destination chain family.
    pub ed25519_group_pubkey_id: u32,
    pub ecdsa_group_pubkey_id: u32,

    /// Source-commitment verification mode (spec §5.4):
    /// - `trust_all`: DEV ONLY — skips the source check.
    /// - `rpc`:       verify the Outbox committed the message at finality
    ///                (not implemented at M1).
    #[serde(default = "default_verifier_mode")]
    pub source_verifier: String,
}

fn default_verifier_mode() -> String {
    "trust_all".to_string()
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        config_load::load_toml(path)
    }

    pub fn ed25519_seed(&self) -> Result<[u8; 32]> {
        parse_seed(&self.ed25519_seed_hex).context("ed25519_seed_hex")
    }

    pub fn secp256k1_seed(&self) -> Result<[u8; 32]> {
        parse_seed(&self.secp256k1_seed_hex).context("secp256k1_seed_hex")
    }
}

fn parse_seed(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("decoding hex seed")?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("seed must be 32 bytes, got {}", v.len()))
}
