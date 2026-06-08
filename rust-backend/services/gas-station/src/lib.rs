//! gas-station.
//!
//! Sponsors user transactions: the frontend sends a `GasLessTransactionData`
//! (a BCS `TransactionKind`), the station attaches the sponsor wallet's gas,
//! dry-runs to size the budget, signs with the sponsor key (loaded from the
//! workspace secrets file, sourced from AWS Secrets Manager in deployed envs),
//! and returns the full `TransactionData` plus the sponsor signature. The
//! user's wallet co-signs the same bytes; both signatures execute together.
//!
//! Endpoints (all on one public port, proxied by nginx):
//! - `GET /health`
//! - `GET /balance` — the sponsor wallet's SUI balance + a health flag.
//! - `POST /sponsor` — sign a gasless transaction.

pub mod config;
pub mod handlers;
pub mod router;
pub mod state;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "gas-station",
    about = "Sponsors user transactions by paying their gas with a sponsor wallet."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/gas-station/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML holding the sponsor `[sui]` key. No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/gas-station/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "gas-station",
    cargo_pkg   = "gas-station",
    working_dir = ".",
    description = "Gas station. Sponsors user transactions by attaching the sponsor wallet's \
                   gas to a gasless TransactionKind, dry-running for budget, and co-signing.",
    cli         = crate::Cli,
}
