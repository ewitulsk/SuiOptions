//! HTTP backend for the frontend.
//!
//! Every read is a just-in-time GraphQL query to the indexer — api-service
//! holds no protocol state of its own. Holds no funds, signs nothing —
//! strictly a read/query layer.

pub mod bucket;
pub mod catalog;
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
    name = "api-service",
    about = "HTTP backend that mirrors indexer state and serves it to the frontend."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/api-service/config/config.toml")]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "api-service",
    cargo_pkg   = "api-service",
    working_dir = ".",
    description = "HTTP backend for the frontend. Serves protocol state via REST, \
                   sourced from just-in-time GraphQL queries to the indexer.",
    cli         = crate::Cli,
}
