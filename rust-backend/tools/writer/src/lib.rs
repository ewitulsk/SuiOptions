//! Library surface for the `writer` binary.
//!
//! Hosts the clap [`Cli`] type and the [`program_spec`] entry point so the
//! control-panel TUI can introspect the binary's flags without exec'ing it.
//! The actual writer flow lives in `main.rs`.

use std::path::PathBuf;

use clap::Parser;
use sui_types::base_types::ObjectID;

use sui_tx::sui_client::Network;

#[derive(Parser, Debug)]
#[command(name = "writer", about = "Retail-writer test client for the options protocol")]
pub struct Cli {
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback.
    #[arg(short = 's', long, default_value = "tools/writer/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(short = 'q', long, default_value = "ws://127.0.0.1:9002/")]
    pub quoting_url: String,

    /// Bucket id we're writing into.
    #[arg(short, long)]
    pub bucket: ObjectID,

    /// Underlying amount we're writing, in raw smallest-units (see token
    /// decimals in the token-info service's testTokens).
    #[arg(short = 'w', long)]
    pub write_amount: u64,

    /// Symbol for the underlying token (TBTC, TDEEP, TUSDC, TWAL).
    #[arg(long, default_value = "TBTC")]
    pub underlying: String,

    /// Symbol for the settlement token.
    #[arg(long, default_value = "TUSDC")]
    pub settlement: String,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    #[arg(long, default_value_t = 5)]
    pub rfq_timeout_secs: u64,
}

cli_spec::define_program! {
    id          = "writer",
    cargo_pkg   = "writer",
    working_dir = ".",
    description = "Retail-writer test client. Walks the §8.1 writer flow end to end: RFQ \
                   over WS, pick best quote, submit one PTB that mints underlying via the \
                   faucet and calls bucket::execute_write.",
    cli         = crate::Cli,
}
