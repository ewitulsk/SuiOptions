//! Library surface for the `cctp-relay` binary.
//!
//! Circle CCTP v1 bridge relay between Sui and Solana: accepts burn tx
//! hashes over HTTP, polls Circle's attestation API (iris) for every
//! pending transfer, auto-submits the destination-chain mint with
//! service-held keys, and tracks end-to-end bridge duration
//! (burned_at → minted_at).

use std::path::PathBuf;

use clap::Parser;

pub mod config;
pub mod db;
pub mod iris;
pub mod message;
pub mod relayer;
pub mod router;
pub mod solana_mint;
pub mod solana_rpc;
pub mod state;
pub mod sui_mint;
pub mod watcher;

/// CCTP v1 domain ids.
pub const DOMAIN_SUI: u32 = 8;
pub const DOMAIN_SOLANA: u32 = 5;

#[derive(Parser, Debug)]
#[command(name = "cctp-relay", about = "Circle CCTP v1 Sui<->Solana bridge relay")]
pub struct Cli {
    #[arg(short, long, default_value = "services/cctp-relay/config/config.toml")]
    pub config: PathBuf,

    /// Secrets TOML with the relayer keys (`[sui]` + `[solana]`) rendered by
    /// render-secrets.sh. Required — the relayer cannot mint without keys.
    #[arg(long)]
    pub secrets: PathBuf,
}

cli_spec::define_program! {
    id          = "cctp-relay",
    cargo_pkg   = "cctp-relay",
    working_dir = ".",
    description = "Tracks Circle CCTP v1 USDC transfers between Sui and Solana, polls the \
                   attestation API, and auto-relays the destination-chain mint.",
    cli         = crate::Cli,
}
