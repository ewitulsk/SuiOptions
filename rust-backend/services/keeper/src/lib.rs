//! Library surface for the vault-keeper binary.
//!
//! The keeper is the permissionless crank-driver for the covered-call
//! vaults (README in this directory is the build spec): every tick it
//! reads each configured vault's chain state, decides the one action the
//! round needs next ([`planner`]), and submits it with a Pyth price
//! update prepended in the same PTB ([`submit`]). It holds only a gas
//! wallet — `vault.move` validates everything that matters.

use std::path::PathBuf;

use clap::Parser;

use sui_tx::sui_client::Network;

pub mod config;
pub mod discovery;
pub mod planner;
pub mod slicing;
pub mod state;
pub mod strike;
pub mod submit;

#[derive(Parser, Debug)]
#[command(
    name = "keeper",
    about = "Permissionless covered-call vault crank: redeems positions, selects buckets, \
             opens/settles RFQ slices, swaps proceeds, and finalizes rounds."
)]
pub struct Cli {
    #[arg(short, long, default_value = "services/keeper/config/config.toml")]
    pub config: PathBuf,

    /// Base URL of the token-info service: protocol ids + the
    /// supported-token catalog (coin types, decimals, Pyth feeds).
    #[arg(long, env = "TOKEN_INFO_URL", default_value = "http://127.0.0.1:9005")]
    pub token_info_url: String,

    /// Base URL of the oracle-service: spot prices + realized vol (the single
    /// Pyth gateway). The keeper still hits Hermes directly for the on-chain
    /// VAA, but reads spot/σ from here.
    #[arg(long, env = "ORACLE_URL", default_value = "http://127.0.0.1:9013")]
    pub oracle_url: String,

    /// Per-binary secrets TOML holding the Sui signing key. Any funded
    /// wallet works — the keeper holds no capability objects.
    #[arg(short = 's', long, default_value = "services/keeper/config/secrets.toml")]
    pub secrets: PathBuf,

    #[arg(short, long, value_enum, default_value_t = Network::Testnet)]
    pub network: Network,

    #[arg(long, default_value_t = 200_000_000)]
    pub gas_budget: u64,

    /// Full planning every tick, log the intents, submit nothing.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

cli_spec::define_program! {
    id          = "keeper",
    cargo_pkg   = "keeper",
    working_dir = ".",
    description = "Permissionless covered-call vault crank. Drives each configured vault's \
                   weekly round: crank_redeem, select_bucket, open_rfq, settle_rfq, \
                   swap_proceeds, finalize_round — with Pyth price updates prepended in-PTB.",
    cli         = crate::Cli,
}
