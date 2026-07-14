//! Solana quoting service — the near-clone of `services/quoting-service`
//! for the Solana port (docs/solana/backend/05-solana-quoting-service.md).
//!
//! Pure stateful WS router between retail clients and market makers — holds
//! no funds, signs no transactions. The on-chain protocol is the safety net:
//! oversubscribed MMs revert at execution, the reputation system catches up
//! over many such reverts.
//!
//! Deltas vs the Sui twin:
//!
//! - `protocol_id` is the options_core **Config PDA** (base58), fetched from
//!   solana-token-info at boot.
//! - Canonical quote bytes are **Borsh** of [`quote::SolanaQuote`] (the Sui
//!   BCS replacement); `RFQResponse` entries carry them as `quote_bytes_b64`
//!   so clients can build the Ed25519SigVerify precompile instruction.
//! - **ed25519 only** (program v1): auth challenge and quote signatures
//!   verify with ed25519-dalek; any other scheme is fatal.
//! - Ids are base58 pubkey strings; per-mint balances replace coin types.
//!
//! Internal shape:
//!
//! - [`state`] owns the only mutable state this service keeps locally — the
//!   live reservation table and MM reputation. Account balances, signing
//!   keys, and bucket state are read just-in-time from the solana-indexer's
//!   GraphQL API.
//! - [`rfq`] orchestrates one RFQ end to end: broadcast to MMs, collect with
//!   a deadline, validate, reserve, sort, ship to retail.
//! - [`ws`] is the transport. It owns no state — every interesting decision
//!   happens in [`state`] or [`rfq`].

pub mod coding;
pub mod config;
pub mod errors;
pub mod messages;
pub mod quote;
pub mod rfq;
pub mod state;
pub mod ws;

pub use config::Config;
pub use errors::ServiceError;
pub use state::AppState;

use std::path::PathBuf;

use clap::Parser;

/// CLI flags for the Solana quoting service. Mirrors the Sui twin so the
/// control-panel TUI can drive the service via flags like every other
/// binary.
#[derive(Parser, Debug)]
#[command(
    name = "solana-quoting-service",
    about = "Stateful WS router between retail clients and Solana market makers."
)]
pub struct Cli {
    /// Path to the TOML config. Overrides the `CONFIG_PATH` env var.
    #[arg(
        short,
        long,
        default_value = "services/solana-quoting-service/config/config.toml"
    )]
    pub config: PathBuf,
}

cli_spec::define_program! {
    id          = "solana-quoting-service",
    cargo_pkg   = "solana-quoting-service",
    working_dir = ".",
    description = "Stateful WS router between retail frontends and Solana market-maker bots. \
                   Authenticates MMs via ed25519 signature challenge, brokers RFQs with a \
                   deadline, validates signed Borsh quotes, tracks reservations, scores \
                   reputation.",
    cli         = crate::Cli,
}
