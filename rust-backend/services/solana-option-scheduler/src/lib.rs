//! Library surface for the `solana-option-scheduler` binary.
//!
//! Hosts the clap [`Cli`] and the [`program_spec`] hook so the control-panel
//! TUI can introspect the binary's flags without exec'ing it. The actual
//! tick loop lives in `main.rs` and the supporting modules.

use std::path::PathBuf;

use clap::Parser;

use solana_tx::Network;

pub mod config;
pub mod db;
pub mod families;
pub mod reconcile;
pub mod roller;
pub mod salt;
pub mod schedule;
pub mod spot;
pub mod strike_grid;
pub mod vault_roller;

#[derive(Parser, Debug)]
#[command(
    name = "solana-option-scheduler",
    about = "Bucket creation lifecycle bot (Solana). Rolls new option-bucket families per \
             (underlying, settlement) pair as expiries approach, and auto-provisions \
             covered-call vaults. Holds the options_core admin keypair."
)]
pub struct Cli {
    #[arg(
        short,
        long,
        default_value = "services/solana-option-scheduler/config/config.toml"
    )]
    pub config: PathBuf,

    /// Base URL of the solana-token-info service. The supported-token
    /// catalog and the deployed programs' ids are fetched from here at boot.
    /// Hard cutover: no solana-deployments.json fallback — the binary
    /// crashes if solana-token-info is unreachable.
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the solana-oracle-service: spot prices + realized vol
    /// (the single Pyth gateway).
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Per-binary secrets TOML. Holds the Solana admin keypair in the
    /// `[solana]` block (per-network slots + `default` fallback) and the
    /// optional `rpc_url` override. No env-var fallback.
    #[arg(
        short = 's',
        long,
        default_value = "services/solana-option-scheduler/config/secrets.toml"
    )]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Devnet)]
    pub network: Network,

    /// Log every roll that would be submitted, but don't actually send
    /// `create_bucket` / `create_vault`.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

cli_spec::define_program! {
    id          = "solana-option-scheduler",
    cargo_pkg   = "solana-option-scheduler",
    working_dir = ".",
    description = "Owns the Solana protocol's bucket-creation lifecycle. Rolls create_bucket \
                   families per (underlying, settlement) pair when the latest family is inside \
                   the roll-threshold window, and auto-provisions covered-call vaults. Holds \
                   the options_core admin keypair.",
    cli         = crate::Cli,
}
