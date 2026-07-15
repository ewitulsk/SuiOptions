//! Quoting service (§5).
//!
//! Pure stateful WS router between retail clients and market makers — holds
//! no funds, signs no transactions. The on-chain protocol is the safety net:
//! oversubscribed MMs revert at execution, the reputation system catches up
//! over many such reverts.
//!
//! Internal shape:
//!
//! - [`state`] owns the only mutable state this service keeps locally — the
//!   seen-nonce table and MM reputation. Signing keys and bucket state are
//!   read just-in-time from the indexer's GraphQL API. There is NO balance
//!   or reservation tracking (collateral abstraction, plan §7).
//! - [`rfq`] orchestrates one RFQ end to end: broadcast to MMs, collect with
//!   a deadline, validate, sort, ship to retail.
//! - [`ws`] is the transport. It owns no state — every interesting decision
//!   happens in [`state`] or [`rfq`].

pub mod config;
pub mod errors;
pub mod rfq;
pub mod state;
pub mod ws;

pub use config::Config;
pub use errors::ServiceError;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

/// CLI flags for the quoting service. Mirrors the historical `CONFIG_PATH`
/// env var so the control-panel TUI can drive the service via flags like
/// every other binary.
#[derive(Parser, Debug)]
#[command(
    name = "quoting-service",
    about = "Stateful WS router between retail clients and market makers."
)]
pub struct Cli {
    /// Path to the TOML config. Overrides the `CONFIG_PATH` env var.
    #[arg(short, long, default_value = "services/quoting-service/config/config.toml")]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "quoting-service",
    cargo_pkg   = "quoting-service",
    working_dir = ".",
    description = "Stateful WS router between retail frontends and market-maker bots. \
                   Authenticates MMs via signature challenge, brokers RFQs with a deadline, \
                   validates signed quotes, scores reputation.",
    cli         = crate::Cli,
}
