//! solana-gas-station.
//!
//! Sponsors users' Solana transaction fees — the Solana twin of
//! `services/gas-station` (the Kora/Octane self-hosted fee-payer pattern,
//! see docs/solana/backend/08-solana-gas-station.md). The frontend builds
//! a `VersionedTransaction` with `feePayer =` the station's pubkey, POSTs
//! it here, the station validates it against the sponsored-flow templates,
//! simulates it to bound its own lamport exposure, co-signs the fee-payer
//! slot, and hands it back; the wallet adds the user signature and the
//! frontend submits.
//!
//! Endpoints (one public port, proxied by nginx):
//! - `GET /health`
//! - `GET /balance` — the station wallet's SOL balance + a health flag.
//! - `POST /sponsor` — validate + co-sign a fee-payer transaction.
//! - `POST /faucet` — mint test tokens to a recipient (non-mainnet only;
//!   the station key is the test mints' mint authority).

pub mod config;
pub mod faucet;
pub mod handlers;
pub mod router;
pub mod sponsor;
pub mod state;
pub mod template;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

/// CLI flags. Mirrors the other services so ops tooling drives every
/// binary the same way.
#[derive(Parser, Debug)]
#[command(
    name = "solana-gas-station",
    about = "Sponsors user transactions by co-signing the fee-payer slot with the station wallet."
)]
pub struct Cli {
    /// Path to the TOML config.
    #[arg(
        short,
        long,
        default_value = "services/solana-gas-station/config/config.toml"
    )]
    pub config: PathBuf,

    /// Secrets TOML holding the station `[solana]` keypair (fee payer =
    /// faucet mint authority). No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/solana-gas-station/config/secrets.toml"
    )]
    pub secrets: PathBuf,
}
