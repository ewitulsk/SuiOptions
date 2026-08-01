//! Library surface for the keeper binaries.
//!
//! The keeper is the permissionless liveness layer for the curated
//! trading vaults ([`trading_vault`]): every tick it settles finished
//! auctions, redeems expired positions, sweeps custody, posts external
//! equity ([`venue_equity`]) and fulfills withdrawals. It holds only a
//! gas wallet — the contracts validate everything that matters.
//!
//! The covered-call ("Ribbon-style") vault crank is deprecated (SO-332).
//! Its modules ([`discovery`], [`planner`], [`slicing`], [`state`],
//! [`strike`], [`submit`]) are still here and still compile, but they are
//! driven only by [`legacy_vault`] behind the undeployed `keeper-legacy`
//! binary. The deployed `keeper` never touches them.

use std::path::PathBuf;

use clap::Parser;

use sui_tx::sui_client::Network;

pub mod config;
pub mod discovery;
/// DEPRECATED (SO-332): the covered-call vault crank, reachable only via the
/// undeployed `keeper-legacy` binary. See the module docs.
pub mod legacy_vault;
pub mod planner;
pub mod slicing;
pub mod state;
pub mod strike;
pub mod submit;
pub mod trading_vault;
pub mod venue_equity;

#[derive(Parser, Debug)]
#[command(
    name = "keeper",
    about = "Permissionless trading-vault crank: settles auctions, redeems expired positions, \
             sweeps custody, posts external equity, and fulfills the withdrawal queue."
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
    description = "Permissionless trading-vault crank. Per tick: settle finished RFQ auctions, \
                   redeem expired positions, sweep DeepBook + vault_mm custody, post \
                   external-account equity, and fulfill withdrawals with a composed \
                   attestation-bearing appraisal.",
    cli         = crate::Cli,
}
