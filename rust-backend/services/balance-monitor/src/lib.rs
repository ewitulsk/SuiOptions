//! balance-monitor (SO-180).
//!
//! Watches the SUI balance of the operational wallets (gas-station sponsor,
//! option-scheduler deployer, mm-bot wallet — keeper later) and exports them
//! as Prometheus gauges:
//!
//! - `sui_balance_sui{service, address}` — current balance in whole SUI
//! - `sui_balance_low{service}` — 1 while below the configured threshold
//!
//! While a wallet is below threshold it also emits
//! `error!(alert_id = "low-balance-<service>", ...)` each poll, so both the
//! metric threshold rule and the generic alert_id log rule fire in Grafana.
//!
//! Watched wallets are resolved either from a rendered secrets TOML (the
//! same files the services themselves mount — addresses track key rotation
//! automatically) or from an explicit address (for wallets whose key never
//! lands on this host, e.g. the future keeper).

pub mod config;

pub use config::Config;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "balance-monitor",
    about = "Exports SUI balance gauges + low-balance alerts for the operational wallets."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/balance-monitor/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "balance-monitor",
    cargo_pkg   = "balance-monitor",
    working_dir = ".",
    description = "Watches operational wallet SUI balances; Prometheus gauges + low-balance alerts.",
    cli         = crate::Cli,
}
