//! HTTP backend for the frontend.
//!
//! Subscribes to the indexer's WS fanout, builds an in-memory view of
//! protocol state (currently just buckets), and serves it over HTTP.
//! Holds no funds, signs nothing — strictly a read/query layer.

pub mod bucket;
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

shared::define_program! {
    id          = "api-service",
    cargo_pkg   = "api-service",
    working_dir = ".",
    description = "HTTP backend for the frontend. Subscribes to the indexer's WS fanout, \
                   maintains an in-memory mirror of protocol state, and serves it via REST.",
    cli         = crate::Cli,
}
