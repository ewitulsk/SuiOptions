//! Library surface for the `option-scheduler` binary.
//!
//! Hosts the clap [`Cli`] and the [`program_spec`] hook so the control-panel
//! TUI can introspect the binary's flags without exec'ing it. The actual
//! tick loop lives in `main.rs` and the supporting modules.

use std::path::PathBuf;

use clap::Parser;

use sui_tx::sui_client::Network;

pub mod codegen;
pub mod config;
pub mod db;
pub mod families;
pub mod roller;
pub mod schedule;
pub mod spot;
pub mod strike_grid;

#[derive(Parser, Debug)]
#[command(
    name = "option-scheduler",
    about = "Bucket creation lifecycle bot. Rolls new option-bucket families per (underlying, \
             settlement) pair as expiries approach."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/option-scheduler/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the token-info service. The supported-token catalog and
    /// the protocol's on-chain ids are fetched from here at boot (replaces
    /// reading `deployments.json`). Hard cutover: no fallback — the binary
    /// crashes if token-info is unreachable.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Per-binary secrets TOML. Holds the Sui signing key. No env-var
    /// fallback. Must hold the key that owns the configured OrgCap.
    #[arg(
        short = 's',
        long,
        default_value = "services/option-scheduler/config/secrets.toml"
    )]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    /// Log every roll that would be submitted, but don't actually call
    /// `new_call_option`. Spec calls for this to be the operational default
    /// the first time the bot is pointed at testnet.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

cli_spec::define_program! {
    id          = "option-scheduler",
    cargo_pkg   = "option-scheduler",
    working_dir = ".",
    description = "Owns an org's bucket-creation lifecycle. Tracks live bucket families per \
                   (underlying, settlement) pair via the indexer, and rolls new bucket sets \
                   when the latest family is inside the roll-threshold window. Holds the OrgCap.",
    cli         = crate::Cli,
}
