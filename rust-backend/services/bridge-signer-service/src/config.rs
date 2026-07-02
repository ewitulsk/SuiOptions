use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use bridge_types::message::derive_domain_sep;
use bridge_types::Bytes32;
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

    /// 32-byte hex per-deployment salt; the digest domain separator is
    /// `keccak256("XCHAIN_MSG_V1" || salt)` (spec §2.2). MUST match the deployed
    /// contracts and the relayer.
    pub deployment_salt_hex: String,

    /// Deployment environment. `trust_all` verification is only permitted when
    /// this is `"dev"`; anywhere else the node refuses to start with it (§5.4).
    #[serde(default = "default_environment")]
    pub environment: String,

    /// Source-commitment verification mode (spec §5.4):
    /// - `trust_all`: DEV ONLY — skips the source check.
    /// - `rpc`:       verify the registered Outbox committed the message at
    ///                finality across every configured provider.
    #[serde(default = "default_verifier_mode")]
    pub source_verifier: String,

    /// Per-source-chain RPC config for `source_verifier = "rpc"` (the enclave's
    /// own trusted chain view, spec §5.4). One entry per source chain the node
    /// will sign *for* (i.e. every chain that can be a message's source).
    #[serde(default)]
    pub source_chains: Vec<SourceChainConfig>,

    /// TTL (seconds) after which a completed signing session is evicted (§5.3).
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
    /// Max concurrent signing sessions held in memory (DoS bound).
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Per-IP `POST /sign_requests` budget: `sign_rate_max` per `sign_rate_window_secs`.
    #[serde(default = "default_sign_rate_window_secs")]
    pub sign_rate_window_secs: u64,
    #[serde(default = "default_sign_rate_max")]
    pub sign_rate_max: u32,
}

fn default_lookback_blocks() -> u64 {
    // Conservative: some public RPCs cap eth_getLogs at 1000 blocks/query. A
    // promptly-relayed message is only seconds old, so this is ample; raise it
    // for RPCs that allow wider ranges (or a delayed relay).
    1_000
}

fn default_session_ttl_secs() -> u64 {
    300
}
fn default_max_sessions() -> usize {
    10_000
}
fn default_sign_rate_window_secs() -> u64 {
    60
}
fn default_sign_rate_max() -> u32 {
    120
}

/// A source chain's verification config: which registered Outbox to look for a
/// commitment on, over which independent RPC providers, at what finality.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceChainConfig {
    /// Internal registry id (matches a message's `src_chain_id`).
    pub internal_chain_id: u32,
    /// `"evm"` or `"sui"` — selects the probe + finality semantics.
    pub family: String,
    /// Independent RPC endpoints. §5.4 wants ≥2 unless `allow_single_provider`.
    pub rpc_urls: Vec<String>,
    /// EVM: the registered Outbox contract address (0x, 20 bytes).
    #[serde(default)]
    pub outbox_addr: Option<String>,
    /// Sui: the deployed bridge package id (holds `events::MessageCommitted`).
    #[serde(default)]
    pub package_id: Option<String>,
    /// EVM confirmation depth before a commitment counts as final (§4). Sui uses
    /// deterministic checkpoint finality, so this is ignored there.
    #[serde(default)]
    pub confirmations: u64,
    /// EVM: how many blocks back from head to scan for the commitment. Public
    /// RPCs reject unbounded `eth_getLogs` ranges, and a relayed message is
    /// recent, so we scan a bounded window (default ~10k blocks).
    #[serde(default = "default_lookback_blocks")]
    pub lookback_blocks: u64,
    /// Allow a single RPC provider (dev / Sui-deterministic). Off by default so
    /// production must configure ≥2 independent providers.
    #[serde(default)]
    pub allow_single_provider: bool,
}

fn default_environment() -> String {
    "prod".to_string()
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

    /// Derive the digest domain separator from the configured deployment salt.
    pub fn domain_sep(&self) -> Result<Bytes32> {
        Ok(derive_domain_sep(&parse_seed(&self.deployment_salt_hex).context("deployment_salt_hex")?))
    }
}

fn parse_seed(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("decoding hex seed")?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow!("seed must be 32 bytes, got {}", v.len()))
}
