//! solana-token-info service.
//!
//! The single source of truth for the Solana supported-token catalog and the
//! protocol's on-chain `program_info`. The ONLY service that reads
//! `solana-deployments.json`; every other Solana service and the frontend
//! reads from here (via `solana-token-info-client` / the public HTTP API).
//!
//! Two routers on two ports: a public read API (proxied by nginx) and an
//! internal mutate API (network-isolated). On non-mainnet networks the
//! `/tokens` response overlays the `solana-deployments.json` testTokens at
//! read time.
//!
//! Token identity is the SPL **mint address** (base58 string). There is no
//! normalization anywhere — comparison is byte-exact; the only validation is
//! that a mint base58-decodes to 32 bytes.

pub mod config;
pub mod db;
pub mod handlers;
pub mod overlay;
pub mod router;
pub mod state;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "solana-token-info",
    about = "Solana token catalog + protocol program-info service. Sole reader of solana-deployments.json."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-token-info/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-token-info",
    cargo_pkg   = "solana-token-info",
    working_dir = ".",
    description = "Solana token catalog + protocol program-info service. The sole reader of \
                   solana-deployments.json; serves a public read API and an internal mutate API.",
    cli         = crate::Cli,
}
