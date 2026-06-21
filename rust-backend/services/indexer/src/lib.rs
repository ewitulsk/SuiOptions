//! Indexer (§6).
//!
//! Tails Sui's checkpoint stream via [`sui_data_ingestion_core`], BCS-decodes
//! events emitted by the `options_protocol` Move package, and persists them to
//! Postgres. Consumers read protocol state via the GraphQL query API
//! ([`graphql::serve`]).
//!
//! - [`worker::ProtocolEventWorker`] implements the framework's `Worker`
//!   trait. Pure dispatch lives in [`event_types::dispatch`] so the BCS path
//!   is unit-testable without spinning up the framework.
//! - [`store::Store`] is the in-memory materialized views (accounts, buckets,
//!   positions) — a write-through cache over Postgres.
//! - [`graphql`] serves point/list queries over the Postgres views.

pub mod config;
pub mod db;
pub mod event_types;
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
#[command(name = "indexer", about = "Tails the Sui checkpoint stream and serves a GraphQL query API.")]
pub struct Cli {
    /// Path to the TOML config. Overrides the `CONFIG_PATH` env var.
    #[arg(short, long, default_value = "services/indexer/config/config.toml")]
    pub config: PathBuf,

    /// Optional secrets TOML. Only `[sui] rpc_url` is read — the shared Sui
    /// RPC override rendered by render-secrets.sh. Optional: a missing or
    /// unreadable file falls back to the config / public RPC, so the indexer
    /// never crash-loops when the secret isn't rendered.
    #[arg(long)]
    pub secrets: Option<PathBuf>,
}

cli_spec::define_program! {
    id          = "indexer",
    cargo_pkg   = "indexer",
    working_dir = ".",
    description = "Tails Sui's checkpoint stream, BCS-decodes options-protocol events, \
                   materializes per-account / per-bucket / per-position views in Postgres, \
                   and serves them to consumers over a GraphQL query API.",
    cli         = crate::Cli,
}
