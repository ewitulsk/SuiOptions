//! Indexer (§6).
//!
//! Tails Sui's checkpoint stream via [`sui_data_ingestion_core`], BCS-decodes
//! events emitted by the `options_protocol` Move package, ingests them into
//! [`store::Store`], and fans the live stream out to the quoting service
//! over WS ([`fanout::serve`]).
//!
//! - [`worker::ProtocolEventWorker`] implements the framework's `Worker`
//!   trait. Pure dispatch lives in [`event_types::dispatch`] so the BCS path
//!   is unit-testable without spinning up the framework.
//! - [`store::Store`] is the in-memory event log + materialized views
//!   (accounts, buckets, positions) + tokio broadcast channel.
//! - [`fanout`] serves snapshots from the log and live events from the
//!   broadcast.

pub mod config;
pub mod db;
pub mod event_types;
pub mod fanout;
pub mod graphql;
pub mod progress;
pub mod store;
pub mod worker;

pub use config::Config;
pub use db::{establish_pool, run_migrations, Repo};
pub use event_types::EventTypes;
pub use progress::{ProgressSnapshot, ProgressState};
pub use store::{AccountState, BucketState, PositionState, Store};
pub use worker::ProtocolEventWorker;

use std::path::PathBuf;

use clap::Parser;

/// CLI flags for the indexer. Mirrors what the binary historically read from
/// the `CONFIG_PATH` environment variable so the control-panel TUI can drive
/// the service the same way it drives any other binary.
#[derive(Parser, Debug)]
#[command(name = "indexer", about = "Tails the Sui checkpoint stream and serves an event WS fanout.")]
pub struct Cli {
    /// Path to the TOML config. Overrides the `CONFIG_PATH` env var.
    #[arg(short, long, default_value = "services/indexer/config/config.toml")]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "indexer",
    cargo_pkg   = "indexer",
    working_dir = ".",
    description = "Tails Sui's checkpoint stream, BCS-decodes options-protocol events, \
                   materializes per-account / per-bucket / per-position views, and exposes \
                   the live stream over a WS fanout for the quoting service.",
    cli         = crate::Cli,
}
