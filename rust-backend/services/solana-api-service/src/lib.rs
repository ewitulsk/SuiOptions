//! HTTP backend for the Solana frontend.
//!
//! The Solana twin of `services/api-service`: every read is a just-in-time
//! GraphQL query to solana-indexer — the service holds no protocol state of
//! its own. Holds no funds, signs nothing — strictly a read/query layer.
//! (One exception: a read-only `getAccountInfo` for live vault round state;
//! see [`solana_rpc`].)

pub mod catalog;
pub mod config;
pub mod handlers;
pub mod ids;
pub mod router;
pub mod solana_rpc;
pub mod state;

pub use config::Config;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "solana-api-service",
    about = "HTTP backend that mirrors solana-indexer state and serves it to the frontend."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-api-service/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-api-service",
    cargo_pkg   = "solana-api-service",
    working_dir = ".",
    description = "HTTP backend for the Solana frontend. Serves protocol state via REST, \
                   sourced from just-in-time GraphQL queries to solana-indexer.",
    cli         = crate::Cli,
}
