//! solana-balance-monitor — the Solana twin of services/balance-monitor.
//!
//! Watches the SOL balance of the Solana operational wallets (gas-station
//! fee payer, scheduler, keeper, mm-bot) and exports them as Prometheus
//! gauges:
//!
//! - `sol_balance_sol{service, address}` — current balance in whole SOL
//! - `sol_balance_low{service}` — 1 while below the configured threshold
//!
//! While a wallet is below threshold it also emits
//! `error!(alert_id = "low-balance-<service>", ...)` each poll, so both the
//! metric threshold rule and the generic alert_id log rule fire in Grafana.
//! Service names are already `solana-*`, so the alert ids stay unique
//! against the Sui monitor's.
//!
//! Watched wallets are resolved either from a rendered secrets TOML (the
//! same files the services themselves mount — addresses track key rotation
//! automatically) or from an explicit base58 address (for wallets whose key
//! never lands on this host).

pub mod config;

pub use config::Config;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "solana-balance-monitor",
    about = "Exports SOL balance gauges + low-balance alerts for the Solana operational wallets."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-balance-monitor/config/config.toml"
    )]
    pub config: PathBuf,

    /// Optional secrets TOML. Only `[solana] rpc_url` is read — the shared
    /// Solana RPC override rendered by render-secrets.sh. Separate from the
    /// per-watch secrets files used to resolve wallet addresses. Optional: a
    /// missing or unreadable file falls back to the public RPC for the
    /// configured network.
    #[arg(long)]
    pub secrets: Option<PathBuf>,
}
