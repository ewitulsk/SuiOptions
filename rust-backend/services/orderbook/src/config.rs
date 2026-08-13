//! Orderbook service configuration.
//!
//! Loaded from a TOML file (default `config/config.toml`), following the
//! indexer's pattern. Markets and the exchange package id are NOT in this
//! file — they come from `deployments.json` (`exchange` block, written by
//! `deployment-manager --deploy-exchange` and the market-creation
//! ceremony), so a redeploy never requires editing service config.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Deserialize;
use sui_tx::Network;

#[derive(Parser, Debug)]
#[command(name = "orderbook", about = "Hybrid exchange off-chain orderbook service")]
pub struct Cli {
    /// Path to the service config TOML.
    #[arg(long, env = "CONFIG_PATH", default_value = "services/orderbook/config/config.toml")]
    pub config: PathBuf,

    /// Per-binary secrets TOML (relayer signing key + endpoint overrides).
    /// Optional: without it (or without a key for the configured network)
    /// the service runs open-orderbook mode only.
    #[arg(long, default_value = "services/orderbook/config/secrets.toml")]
    pub secrets: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `mainnet` or `testnet` — picks default gRPC/GraphQL endpoints and
    /// the signing-key slot in secrets.toml.
    pub network: String,

    /// Deployment environment key in deployments.json (`staging` / `prod`).
    pub env: String,

    /// Path to deployments.json (relative to the working directory).
    #[serde(default = "default_deployments")]
    pub deployments: String,

    /// Postgres connection string.
    pub database_url: String,

    /// REST/WS bind address.
    pub bind: SocketAddr,

    /// gRPC endpoint override (else the secrets `[sui] grpc_url`, else the
    /// network's public fullnode).
    #[serde(default)]
    pub grpc_url: Option<String>,

    /// GraphQL endpoint override for event reads (same precedence).
    #[serde(default)]
    pub graphql_url: Option<String>,

    /// Per-settlement gas budget in MIST.
    #[serde(default = "default_gas_budget")]
    pub gas_budget: u64,

    /// r2d2 pool size.
    #[serde(default = "default_pool_size")]
    pub db_pool_size: u32,
}

fn default_deployments() -> String {
    "deployments.json".to_owned()
}

fn default_gas_budget() -> u64 {
    50_000_000
}

fn default_pool_size() -> u32 {
    16
}

/// Ids the direct-vault-escrow paths need (SO-372), from the same
/// deployments record as the markets. Absent (older records, or no
/// trading-vault deploy) ⇒ direct escrow is disabled and every manager is
/// treated as a plain wallet BM.
#[derive(Debug, Clone)]
pub struct DirectEscrowIds {
    /// exchange_adapter package id, `0x`-hex.
    pub adapter_package: String,
    /// Shared trading-vault `IntegrationRegistry` (read-only in fill PTBs).
    pub integration_registry_id: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        runtime_config::config_load::load_toml(path)
    }

    pub fn network(&self) -> Result<Network> {
        match self.network.to_ascii_lowercase().as_str() {
            "mainnet" => Ok(Network::Mainnet),
            "testnet" => Ok(Network::Testnet),
            other => Err(anyhow!("unsupported network {other} (mainnet|testnet)")),
        }
    }

    /// Markets from the deployments record's `exchange` block. Token type
    /// strings are canonicalized here once; everything downstream compares
    /// canonical forms only (move-type-normalization rule).
    pub fn load_markets(
        &self,
    ) -> Result<(deployments::ExchangeInfo, Vec<exchange_types::Market>)> {
        let all = deployments::Deployments::load(Path::new(&self.deployments))
            .with_context(|| format!("loading {}", self.deployments))?;
        let dep = all
            .for_env(&self.env)
            .with_context(|| format!("env {} in {}", self.env, self.deployments))?;
        let exchange = dep.exchange()?.clone();
        let mut markets = Vec::new();
        for (symbol, m) in &exchange.markets {
            markets.push(exchange_types::Market {
                symbol: symbol.clone(),
                registry_id: exchange_types::SuiAddress::parse(&m.registry_id)
                    .map_err(|e| anyhow!("market {symbol}: {e}"))?,
                base: exchange_types::canonicalize_move_type(&m.base)
                    .map_err(|e| anyhow!("market {symbol}: {e}"))?,
                quote: exchange_types::canonicalize_move_type(&m.quote)
                    .map_err(|e| anyhow!("market {symbol}: {e}"))?,
                tick_size: m.tick_size,
                min_size: m.min_size,
                lot_size: m.lot_size,
                current_fee_bps: m.fee_bps,
            });
        }
        Ok((exchange, markets))
    }

    /// The standalone ingress whitelist record (SO-384): the shared
    /// `Whitelist` object every fill/match entry takes right after the
    /// registry. `None` on records predating the standalone package.
    pub fn load_whitelist(&self) -> Result<Option<deployments::WhitelistInfo>> {
        let all = deployments::Deployments::load(Path::new(&self.deployments))
            .with_context(|| format!("loading {}", self.deployments))?;
        let dep = all
            .for_env(&self.env)
            .with_context(|| format!("env {} in {}", self.env, self.deployments))?;
        Ok(dep.package_info.whitelist.clone())
    }

    /// Direct-escrow ids from the deployments record (SO-372). `None` when
    /// the record has no exchange_adapter or trading-vault-objects block.
    pub fn load_direct_escrow(&self) -> Result<Option<DirectEscrowIds>> {
        let all = deployments::Deployments::load(Path::new(&self.deployments))
            .with_context(|| format!("loading {}", self.deployments))?;
        let dep = all
            .for_env(&self.env)
            .with_context(|| format!("env {} in {}", self.env, self.deployments))?;
        let pi = &dep.package_info;
        Ok(match (&pi.exchange_adapter, &pi.trading_vault_objects) {
            (Some(ea), Some(objs)) => Some(DirectEscrowIds {
                adapter_package: ea.package_id.clone(),
                integration_registry_id: objs.integration_registry_id.clone(),
            }),
            _ => None,
        })
    }
}
