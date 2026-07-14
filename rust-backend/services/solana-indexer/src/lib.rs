//! Solana indexer.
//!
//! Ingests the three options programs' `emit_cpi!` events from a Helius
//! LaserStream (Yellowstone gRPC) subscription at `confirmed` commitment,
//! persists them to Postgres with a `finalized` reorg watermark, and
//! serves consumers over the same GraphQL surface conventions as the Sui
//! indexer (`POST /graphql` + `GET /progress` on :9002, ops on :8081).
//!
//! - [`events`] — Borsh+serde mirror structs for all 50 Anchor events,
//!   discriminator registry, typed [`events::DecodedEvent`] union.
//! - [`decode`] — transaction → events (inner-instruction walk).
//! - [`worker`] — the stream loop: per-slot batching, finalized
//!   watermark, fork eviction backstop.
//! - [`db`] — event log + materialised views; folds are gated on event
//!   insertion so replays are idempotent end-to-end.
//! - [`graphql`] — the query API.

pub mod config;
pub mod db;
pub mod decode;
pub mod events;
pub mod graphql;
pub mod progress;
pub mod worker;

pub use config::{Config, Secrets};
pub use db::{establish_pool, run_migrations, Repo};
pub use progress::{ProgressSnapshot, ProgressState};

use std::path::PathBuf;

use clap::Parser;

/// CLI flags. Mirrors the other services so ops tooling drives every
/// binary the same way.
#[derive(Parser, Debug)]
#[command(
    name = "solana-indexer",
    about = "Ingests options-program events from Helius LaserStream and serves a GraphQL query API."
)]
pub struct Cli {
    /// Path to the TOML config.
    #[arg(
        short,
        long,
        default_value = "services/solana-indexer/config/config.toml"
    )]
    pub config: PathBuf,

    /// Secrets TOML rendered by render-secrets.sh (`[helius] api_key`).
    #[arg(long)]
    pub secrets: Option<PathBuf>,
}
