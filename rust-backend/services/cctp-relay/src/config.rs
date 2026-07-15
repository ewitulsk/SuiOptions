//! Service config. Loaded via `runtime_config::config_load` so `${DB_HOST}` /
//! `${DB_PASSWORD}` expand from the environment at boot.

use std::net::SocketAddr;

use anyhow::Result;
use serde::Deserialize;

fn default_db_pool_size() -> u32 {
    4
}
fn default_poll_interval_secs() -> u64 {
    10
}
fn default_relay_interval_secs() -> u64 {
    10
}
fn default_max_mint_attempts() -> i32 {
    5
}
fn default_origins() -> Vec<String> {
    vec!["*".to_string()]
}
fn default_sui_gas_budget() -> u64 {
    100_000_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub environment: String,
    pub bind_addr: SocketAddr,

    /// Shared RDS Postgres, assembled from `${DB_HOST}` / `${DB_PASSWORD}`.
    pub database_url: String,
    #[serde(default = "default_db_pool_size")]
    pub db_pool_size: u32,

    /// Circle attestation API base: `https://iris-api-sandbox.circle.com`
    /// (testnet/devnet) or `https://iris-api.circle.com` (mainnet).
    pub iris_base_url: String,

    /// Seconds between attestation-poll ticks.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Seconds between mint-relay ticks.
    #[serde(default = "default_relay_interval_secs")]
    pub relay_interval_secs: u64,
    /// Mint submissions per transfer before it is marked failed (alerts).
    #[serde(default = "default_max_mint_attempts")]
    pub max_mint_attempts: i32,

    #[serde(default = "default_origins")]
    pub allowed_origins: Vec<String>,

    pub sui: SuiConfig,
    pub solana: SolanaConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SuiConfig {
    /// `testnet` | `mainnet` — selects the relayer key slot and public RPC.
    pub network: sui_tx::sui_client::Network,
    /// Circle MessageTransmitter package id.
    pub message_transmitter_package: String,
    /// Circle TokenMessengerMinter package id.
    pub token_messenger_minter_package: String,
    /// Shared MessageTransmitterState object id.
    pub message_transmitter_state: String,
    /// Shared TokenMessengerMinterState object id.
    pub token_messenger_minter_state: String,
    /// Shared USDC Treasury object id.
    pub usdc_treasury: String,
    /// Full USDC coin type (`0x…::usdc::USDC`).
    pub usdc_coin_type: String,
    #[serde(default = "default_sui_gas_budget")]
    pub gas_budget: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SolanaConfig {
    /// `devnet` | `mainnet` — selects the relayer key slot.
    pub network: String,
    pub rpc_url: String,
    /// USDC mint address.
    pub usdc_mint: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        runtime_config::config_load::load_toml(path)
    }
}
